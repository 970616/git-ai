use crate::authorship::attribution_tracker::LineAttribution;
use crate::authorship::authorship_log::{HumanRecord, LineRange, PromptRecord, SessionRecord};
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::hunk_shift::{DiffHunk, apply_hunk_shifts_to_line_attributions};
use crate::authorship::rewrite::compute_diff_trees_batch;
use crate::error::GitAiError;
use crate::git::notes_api;
use crate::git::repository::{Repository, batch_read_paths_at_treeishes};
use std::collections::HashMap;

/// Handles working log reconstruction after a backward reset (e.g. git reset --mixed HEAD~N).
///
/// After reset, HEAD is at new_tip but working tree still has content from old_tip.
/// We need to reconstruct working log entries from the authorship notes of the
/// "un-done" commits so that the next commit preserves AI attribution.
pub fn reconstruct_working_log_after_backward_reset(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
) -> Result<(), GitAiError> {
    // List all commits being "un-done" (between new_tip exclusive and old_tip inclusive)
    let commits = list_commits_in_range(repo, new_tip, old_tip);
    if commits.is_empty() {
        tracing::warn!(
            "reset reconstruct: no commits in range {}..{}; nothing to rebuild",
            new_tip,
            old_tip
        );
        return Ok(());
    }

    // Read authorship notes for all un-done commits
    let mut commit_logs: Vec<(String, AuthorshipLog)> = Vec::new();
    let notes = notes_api::read_notes_batch(repo, &commits)?;
    for commit_sha in &commits {
        let Some(raw_note) = notes.get(commit_sha) else {
            continue;
        };
        let Ok(log) = AuthorshipLog::deserialize_from_string(raw_note) else {
            continue;
        };
        commit_logs.push((commit_sha.clone(), log));
    }

    if commit_logs.is_empty() {
        tracing::warn!(
            "reset reconstruct: no usable authorship notes for {} commits ({}..{}); nothing to rebuild",
            commits.len(),
            new_tip,
            old_tip
        );
        return Ok(());
    }

    tracing::info!(
        "reset reconstruct: {} commits in range, {} with usable notes ({}..{})",
        commits.len(),
        commit_logs.len(),
        new_tip,
        old_tip
    );

    // Compute diffs from each intermediate commit to old_tip so we can shift
    // line numbers into old_tip's coordinate space. Commits that ARE old_tip
    // need no shift.
    let diff_pairs: Vec<(String, String)> = commit_logs
        .iter()
        .filter(|(sha, _)| sha != old_tip)
        .map(|(sha, _)| (sha.clone(), old_tip.to_string()))
        .collect();

    let diff_results = if !diff_pairs.is_empty() {
        compute_diff_trees_batch(repo, &diff_pairs)?
    } else {
        Vec::new()
    };

    // Build a lookup from commit SHA to its diff result index
    let diff_idx_by_sha: HashMap<&str, usize> = diff_pairs
        .iter()
        .enumerate()
        .map(|(idx, (sha, _))| (sha.as_str(), idx))
        .collect();

    // Collect attributions from all commits, shifting intermediate ones to old_tip's
    // coordinate space. Process in chronological order (oldest first) so that later
    // commits' attributions override earlier ones for overlapping lines.
    let mut file_attributions: HashMap<String, Vec<LineAttribution>> = HashMap::new();
    let mut prompts: HashMap<String, PromptRecord> = HashMap::new();
    let mut sessions: std::collections::BTreeMap<String, SessionRecord> =
        std::collections::BTreeMap::new();
    let mut humans: std::collections::BTreeMap<String, HumanRecord> =
        std::collections::BTreeMap::new();

    for (commit_sha, log) in &commit_logs {
        let hunks_by_file: Option<&HashMap<String, Vec<DiffHunk>>> = diff_idx_by_sha
            .get(commit_sha.as_str())
            .map(|&idx| &diff_results[idx].hunks_by_file);

        extract_attributions_from_log_shifted(
            log,
            hunks_by_file,
            &mut file_attributions,
            &mut prompts,
            &mut sessions,
            &mut humans,
        );
    }

    if file_attributions.is_empty() {
        tracing::warn!(
            "reset reconstruct: notes had no file attributions ({} commits); nothing to rebuild",
            commit_logs.len()
        );
        return Ok(());
    }

    // Use the content from old_tip (the commit being reset FROM) as the blob snapshot.
    // After a mixed/soft reset, the working tree originally had old_tip's content.
    // We cannot read the working directory here because by the time the daemon processes
    // the reset event, the user may have already modified files further.
    let mut file_blobs: HashMap<String, String> = HashMap::new();
    let mut blob_requests = Vec::new();
    for file_path in file_attributions.keys() {
        blob_requests.push((old_tip.to_string(), file_path.clone()));
        blob_requests.push((new_tip.to_string(), file_path.clone()));
    }
    let tree_contents = batch_read_paths_at_treeishes(repo, &blob_requests)?;
    for file_path in file_attributions.keys() {
        let old_key = (old_tip.to_string(), file_path.clone());
        let Some(content) = tree_contents.get(&old_key) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }

        let new_key = (new_tip.to_string(), file_path.clone());
        if tree_contents.get(&new_key) != Some(content) {
            file_blobs.insert(file_path.clone(), content.clone());
        }
    }

    // If no files differ from the target (reset --hard), nothing to reconstruct
    if file_blobs.is_empty() {
        tracing::warn!(
            "reset reconstruct: no files differ between {} and {} (or old_tip content unreadable); nothing to rebuild",
            old_tip,
            new_tip
        );
        let _ = repo.storage.delete_working_log_for_base_commit(old_tip);
        return Ok(());
    }

    // Only keep attributions for files that have uncommitted content
    file_attributions.retain(|path, _| file_blobs.contains_key(path));

    // Write as INITIAL working log for new_tip.
    // Do NOT call reset_working_log() here: checkpoints may have already been
    // written between the time the reset happened and when the daemon processes
    // this event. Clearing checkpoints.jsonl would lose that data.
    let working_log = repo.storage.working_log_for_base_commit(new_tip)?;

    let rebuilt_file_count = file_blobs.len();

    working_log.write_initial_attributions_with_contents(
        file_attributions,
        prompts,
        humans,
        file_blobs,
        sessions,
    )?;

    // 也把 old_tip 的 working log（未提交的 AI 打点）合并进来。
    // reset --mixed 不清工作区：未提交编辑的打点仍挂在旧 base 目录下，
    // 若不迁移会随旧目录归档而被丢弃，导致"已提交部分恢复成 AI、
    // 未提交部分全部变 human"。这里只补充 new_tip 还没有的路径，
    // 保留上面从 note 重建出的结果。
    if repo.storage.has_working_log(old_tip) {
        if let Ok(old_log) = repo.storage.working_log_for_base_commit(old_tip) {
            let old_initial = old_log.read_initial_attributions();
            let old_checkpoints = old_log.read_all_checkpoints().unwrap_or_default();
            let old_checkpoints_len = old_checkpoints.len();
            tracing::info!(
                "reset reconstruct: old_tip working log {} has {} initial files, {} checkpoints",
                old_tip,
                old_initial.files.len(),
                old_checkpoints_len
            );
            if !old_initial.files.is_empty() {
                let mut target = working_log.read_initial_attributions();
                let mut added = 0usize;
                let dst_blobs = working_log.dir.join("blobs");
                let _ = std::fs::create_dir_all(&dst_blobs);
                for (path, attrs) in old_initial.files.iter() {
                    if target.files.contains_key(path) {
                        continue;
                    }
                    target.files.insert(path.clone(), attrs.clone());
                    if let Some(blob_sha) = old_initial.file_blobs.get(path) {
                        let src = old_log.dir.join("blobs").join(blob_sha);
                        let dst = dst_blobs.join(blob_sha);
                        if src.exists() && !dst.exists() {
                            let _ = std::fs::copy(&src, &dst);
                        }
                        target.file_blobs.insert(path.clone(), blob_sha.clone());
                    }
                    added += 1;
                }
                for (k, v) in old_initial.prompts.iter() {
                    target.prompts.entry(k.clone()).or_insert_with(|| v.clone());
                }
                for (k, v) in old_initial.humans.iter() {
                    target.humans.entry(k.clone()).or_insert_with(|| v.clone());
                }
                for (k, v) in old_initial.sessions.iter() {
                    target.sessions.entry(k.clone()).or_insert_with(|| v.clone());
                }
                tracing::info!(
                    "reset reconstruct: merged {} missing files from old_tip working log {}",
                    added,
                    old_tip
                );
                if added > 0 {
                    // best-effort：合并失败不应破坏上面的重建结果
                    let _ = working_log.write_initial(target);
                }
            }

            // 迁移 old_tip 的编辑打点（checkpoints）：reset --mixed 不清工作区，
            // 未提交编辑的打点仍挂在旧 base 目录下。不迁移的话，reset 后再编辑
            // 并提交时这些行无法归属（实测会被记为 human）；迁移后 note 恢复为 AI。
            if !old_checkpoints.is_empty() {
                let dst_blobs = working_log.dir.join("blobs");
                let _ = std::fs::create_dir_all(&dst_blobs);
                for checkpoint in &old_checkpoints {
                    for entry in &checkpoint.entries {
                        if entry.blob_sha.is_empty() {
                            continue;
                        }
                        let src = old_log.dir.join("blobs").join(&entry.blob_sha);
                        let dst = dst_blobs.join(&entry.blob_sha);
                        if src.exists() && !dst.exists() {
                            let _ = std::fs::copy(&src, &dst);
                        }
                    }
                }
                let mut merged = working_log.read_all_checkpoints().unwrap_or_default();
                merged.extend(old_checkpoints);
                let _ = working_log.write_all_checkpoints(&merged);
                tracing::info!(
                    "reset reconstruct: migrated checkpoints from old_tip working log {}",
                    old_tip
                );
            }
        } else {
            tracing::warn!(
                "reset reconstruct: old_tip working log {} unreadable",
                old_tip
            );
        }
    } else {
        tracing::info!(
            "reset reconstruct: no working log dir for old_tip {}",
            old_tip
        );
    }

    tracing::info!(
        "reset reconstruct: rebuild complete for {} (from {}: {} files, {} commits with notes)",
        new_tip,
        old_tip,
        rebuilt_file_count,
        commit_logs.len()
    );

    // Delete old working log if it exists
    let _ = repo.storage.delete_working_log_for_base_commit(old_tip);

    Ok(())
}

fn extract_attributions_from_log_shifted(
    log: &AuthorshipLog,
    hunks_by_file: Option<&HashMap<String, Vec<DiffHunk>>>,
    file_attributions: &mut HashMap<String, Vec<LineAttribution>>,
    prompts: &mut HashMap<String, PromptRecord>,
    sessions: &mut std::collections::BTreeMap<String, SessionRecord>,
    humans: &mut std::collections::BTreeMap<String, HumanRecord>,
) {
    for fa in &log.attestations {
        let mut raw_attrs: Vec<LineAttribution> = Vec::new();
        for entry in &fa.entries {
            for range in &entry.line_ranges {
                let (start, end) = match range {
                    LineRange::Single(l) => (*l, *l),
                    LineRange::Range(s, e) => (*s, *e),
                };
                raw_attrs.push(LineAttribution::new(start, end, entry.hash.clone(), None));
            }
        }

        // Shift line numbers to old_tip's coordinate space if we have hunks for this file
        let shifted = if let Some(all_hunks) = hunks_by_file
            && let Some(file_hunks) = all_hunks.get(&fa.file_path)
            && !file_hunks.is_empty()
        {
            apply_hunk_shifts_to_line_attributions(&raw_attrs, file_hunks)
        } else {
            raw_attrs
        };

        // Merge into accumulated attributions. Later commits override earlier ones
        // for overlapping line ranges.
        let existing = file_attributions.entry(fa.file_path.clone()).or_default();
        for new_attr in shifted {
            // Remove any existing attributions that are fully covered by this new one
            existing.retain(|old| {
                !(old.start_line >= new_attr.start_line && old.end_line <= new_attr.end_line)
            });
            // For partial overlaps, trim existing attributions. The head and
            // tail trims are INDEPENDENT: when `old` strictly encloses
            // `new_attr` (old.start < new.start AND old.end > new.end) both
            // fragments must survive, so we must not `return false` after the
            // head trim alone -- that would drop the tail [new.end+1, old.end].
            let mut trimmed: Vec<LineAttribution> = Vec::new();
            existing.retain(|old| {
                let head_overlap =
                    old.start_line < new_attr.start_line && old.end_line >= new_attr.start_line;
                let tail_overlap =
                    old.end_line > new_attr.end_line && old.start_line <= new_attr.end_line;

                if !head_overlap && !tail_overlap {
                    // No partial overlap with this `old`; keep it untouched.
                    return true;
                }

                if head_overlap {
                    // Overlap at the end of old — keep old's head before new.
                    trimmed.push(LineAttribution::new(
                        old.start_line,
                        new_attr.start_line - 1,
                        old.author_id.clone(),
                        old.overrode.clone(),
                    ));
                }
                if tail_overlap {
                    // Overlap at the start of old — keep old's tail after new.
                    trimmed.push(LineAttribution::new(
                        new_attr.end_line + 1,
                        old.end_line,
                        old.author_id.clone(),
                        old.overrode.clone(),
                    ));
                }
                // The original `old` is replaced by the fragment(s) above.
                false
            });
            existing.extend(trimmed);
            existing.push(new_attr);
        }
    }

    for (key, record) in &log.metadata.prompts {
        prompts.entry(key.clone()).or_insert_with(|| record.clone());
    }
    for (key, record) in &log.metadata.sessions {
        sessions
            .entry(key.clone())
            .or_insert_with(|| record.clone());
    }
    for (key, record) in &log.metadata.humans {
        humans.entry(key.clone()).or_insert_with(|| record.clone());
    }
}

fn list_commits_in_range(repo: &Repository, base: &str, tip: &str) -> Vec<String> {
    crate::authorship::rewrite::list_commits_in_range(repo, base, tip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log_serialization::{AttestationEntry, FileAttestation};

    fn log_with_single_entry(file: &str, hash: &str, start: u32, end: u32) -> AuthorshipLog {
        let mut log = AuthorshipLog::new();
        let mut fa = FileAttestation::new(file.to_string());
        fa.add_entry(AttestationEntry::new(
            hash.to_string(),
            vec![LineRange::Range(start, end)],
        ));
        log.attestations.push(fa);
        log
    }

    /// Regression (#2): when a later commit's range is strictly enclosed by an
    /// earlier commit's range for the same file, BOTH the head fragment
    /// [old.start, new.start-1] and the tail fragment [new.end+1, old.end] must
    /// survive. The old code's two trim branches were mutually exclusive (the
    /// head branch `return false`d before the tail branch could run), so the
    /// tail was silently dropped.
    #[test]
    fn test_enclosed_range_preserves_head_and_tail() {
        let mut file_attributions: HashMap<String, Vec<LineAttribution>> = HashMap::new();
        let mut prompts = HashMap::new();
        let mut sessions = std::collections::BTreeMap::new();
        let mut humans = std::collections::BTreeMap::new();

        // Oldest commit first: human owns lines 1..=10 of f.txt.
        let old_log = log_with_single_entry("f.txt", "h_old", 1, 10);
        extract_attributions_from_log_shifted(
            &old_log,
            None,
            &mut file_attributions,
            &mut prompts,
            &mut sessions,
            &mut humans,
        );

        // Later commit: AI owns lines 4..=6 (strictly inside the human range).
        let new_log = log_with_single_entry("f.txt", "ai_new", 4, 6);
        extract_attributions_from_log_shifted(
            &new_log,
            None,
            &mut file_attributions,
            &mut prompts,
            &mut sessions,
            &mut humans,
        );

        let mut attrs = file_attributions.remove("f.txt").expect("f.txt present");
        attrs.sort_by_key(|a| a.start_line);

        // Expect three segments: human head [1,3], AI [4,6], human tail [7,10].
        assert_eq!(
            attrs.len(),
            3,
            "enclosed AI range must split the human range into head + tail, got: {:?}",
            attrs
        );
        assert_eq!((attrs[0].start_line, attrs[0].end_line), (1, 3));
        assert_eq!(attrs[0].author_id, "h_old");
        assert_eq!((attrs[1].start_line, attrs[1].end_line), (4, 6));
        assert_eq!(attrs[1].author_id, "ai_new");
        assert_eq!((attrs[2].start_line, attrs[2].end_line), (7, 10));
        assert_eq!(attrs[2].author_id, "h_old");
    }
}
