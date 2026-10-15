//! reset --hard 换基后的"孤儿链内容认领"（重做/再检出/粘贴内容 → 归属找回）。
//!
//! 背景：`git reset --hard` 换基（放弃本地链、对齐远端）时 daemon 按设计删除工作日志
//! （⑩ 语义：--hard 即丢弃工作区）。被切走旧链的归属停留在旧提交的 note 上不再跟随；
//! 若用户随后把旧内容"重做"（重新检出 `git checkout 旧sha -- 文件` / 粘贴）到新链上
//! 提交，这些内容没有任何编辑打点、也不走 cherry-pick/rebase 的 patch-id 继承机制
//! → 全部被判 human（maliyang 案，见 known-issues ㉒）。
//!
//! 本模块提供窄触发的找回机制（普通提交/amend 之外零影响）：
//! 1. daemon 在每次 reset --hard 时把 (old_head, new_head, ts) 登记到
//!    `.git/ai/orphaned-reset-chains.json`（`record_orphaned_reset_chain`）；
//! 2. 后续提交结算时（post_commit 普通/amend 路径共用 `claim_content_from_orphaned_chains`），
//!    若仓库存在该登记，对"提交新增、且没有任何 AI 归属"的行，从被丢弃链提交的
//!    note 中按内容匹配认领 AI 归属：
//!    - 层级 A：新旧文件内容一致（忽略行尾）→ 行号一一映射；
//!    - 层级 B：行集合 Jaccard ≥ 0.5（与第十一批 rename 内容认领同阈值）→
//!      旧文件中"唯一出现"的行内容逐行匹配（歧义行放弃；错搬比不搬更糟）。
//!
//! 没有登记文件的仓库在读文件时直接返回，不产生任何行为或性能影响。

use crate::authorship::authorship_log::LineRange;
use crate::authorship::authorship_log_serialization::{AttestationEntry, AuthorshipLog};
use crate::git::repository::{Repository, exec_git_allow_nonzero};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ORPHAN_CHAINS_FILENAME: &str = "orphaned-reset-chains.json";
/// 登记文件保留上限（超出后丢弃最旧记录）。
const MAX_ORPHAN_RECORDS: usize = 50;
/// 登记记录保留时长（过期清理，防止文件无限增长）。
const ORPHAN_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
/// 单条登记最多回溯的候选提交数（rev-list -n）。
const MAX_CANDIDATES_PER_RECORD: usize = 50;
/// 全部登记合计的候选提交数上限。
const MAX_CANDIDATES_TOTAL: usize = 100;
/// 层级 B 的行集合相似度下限（文件不像时不启用逐行匹配，防巧合行误搬）。
const CLAIM_SIMILARITY_FLOOR: f64 = 0.5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct OrphanRecord {
    old_head: String,
    new_head: String,
    ts: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn records_path(repo: &Repository) -> PathBuf {
    repo.storage.ai_dir.join(ORPHAN_CHAINS_FILENAME)
}

fn load_records_from(path: &Path) -> Vec<OrphanRecord> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut records: Vec<OrphanRecord> = serde_json::from_str(&data).unwrap_or_default();
    let cutoff = now_secs().saturating_sub(ORPHAN_RETENTION_SECS);
    records.retain(|r| r.ts >= cutoff);
    records
}

fn save_records_to(path: &Path, records: &[OrphanRecord]) {
    let start = records.len().saturating_sub(MAX_ORPHAN_RECORDS);
    if let Ok(json) = serde_json::to_string(&records[start..]) {
        let _ = std::fs::write(path, json);
    }
}

/// daemon 在 reset --hard 时登记"被丢弃的链头"。best-effort：任何失败都不影响
/// reset 处理本身（与 delete_working_log 的行为解耦，仅新增一条本地记录）。
pub(crate) fn record_orphaned_reset_chain(repo: &Repository, old_head: &str, new_head: &str) {
    if old_head.is_empty() || new_head.is_empty() || old_head == new_head {
        return;
    }
    let path = records_path(repo);
    let mut records = load_records_from(&path);
    let ts = now_secs();
    if let Some(existing) = records
        .iter_mut()
        .find(|r| r.old_head == old_head && r.new_head == new_head)
    {
        existing.ts = ts;
    } else {
        records.push(OrphanRecord {
            old_head: old_head.to_string(),
            new_head: new_head.to_string(),
            ts,
        });
    }
    save_records_to(&path, &records);
    tracing::info!(
        old_head = %old_head,
        new_head = %new_head,
        "orphaned reset chain recorded (content-claim enabled for subsequent commits)"
    );
}

fn git_stdout(repo: &Repository, args: &[String]) -> Option<String> {
    let mut full = repo.global_args_for_exec();
    full.extend(args.iter().cloned());
    match exec_git_allow_nonzero(&full) {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).to_string()),
        _ => None,
    }
}

fn read_file_at_commit(repo: &Repository, commit_sha: &str, file: &str) -> Option<String> {
    git_stdout(
        repo,
        &[
            "show".to_string(),
            format!("{}:{}", commit_sha, file),
        ],
    )
}

fn is_ancestor_commit(repo: &Repository, ancestor: &str, descendant: &str) -> bool {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "merge-base".to_string(),
        "--is-ancestor".to_string(),
        ancestor.to_string(),
        descendant.to_string(),
    ]);
    exec_git_allow_nonzero(&args)
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// 被丢弃链中"改过目标路径"的候选提交（新 → 旧排列）。
fn orphan_candidates(
    repo: &Repository,
    old_head: &str,
    new_head: &str,
    paths: &[String],
) -> Vec<String> {
    let mut args = vec![
        "rev-list".to_string(),
        "-n".to_string(),
        MAX_CANDIDATES_PER_RECORD.to_string(),
        format!("{}..{}", new_head, old_head),
        "--".to_string(),
    ];
    args.extend(paths.iter().cloned());
    git_stdout(repo, &args)
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn split_lines(content: &str) -> Vec<&str> {
    content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// 旧 note 中该文件的行级归属（行号 → 非 h_ hash；h_ 是"无证据兜底"，不当账搬）。
fn build_line_hash(entries: &[AttestationEntry]) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for entry in entries {
        if entry.hash.starts_with("h_") {
            continue;
        }
        for range in &entry.line_ranges {
            for line in range.expand() {
                map.entry(line).or_insert_with(|| entry.hash.clone());
            }
        }
    }
    map
}

/// 对"目标行"（新文件行号，1-based）尝试从旧文件内容匹配认领归属。
///
/// 返回 `None` = 两个内容不构成"同一份代码"（相似度不足，不认领）；
/// `Some(v)` = 内容匹配成立（v 可能为空 = 匹配了但目标行在旧账里没有 AI 归属）。
fn match_claim_lines(
    new_content: &str,
    old_content: &str,
    target_lines: &[u32],
    old_line_hash: &HashMap<u32, String>,
) -> Option<Vec<(String, u32)>> {
    // 层级 A：整文件一致（忽略行尾）→ 行号一一对应，直接搬。
    if crate::authorship::imara_diff_utils::content_eq_ignoring_line_endings(
        old_content,
        new_content,
    ) {
        let hits: Vec<(String, u32)> = target_lines
            .iter()
            .filter_map(|&l| old_line_hash.get(&l).map(|h| (h.clone(), l)))
            .collect();
        return Some(hits);
    }

    // 层级 B：行集合 Jaccard 相似度门。
    let old_set: HashSet<&str> = old_content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let new_set: HashSet<&str> = new_content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let union = old_set.union(&new_set).count() as f64;
    if union == 0.0 {
        return None;
    }
    let score = old_set.intersection(&new_set).count() as f64 / union;
    if score < CLAIM_SIMILARITY_FLOOR {
        return None;
    }

    // 逐行唯一匹配：行内容在旧文件中"唯一出现"才可信（歧义行放弃）。
    let old_lines = split_lines(old_content);
    let mut unique: HashMap<&str, Option<u32>> = HashMap::new();
    for (idx, line) in old_lines.iter().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let line_no = (idx + 1) as u32;
        let slot = unique.entry(t).or_insert(Some(line_no));
        if *slot != Some(line_no) {
            *slot = None;
        }
    }

    let new_lines = split_lines(new_content);
    let mut hits: Vec<(String, u32)> = Vec::new();
    for &l in target_lines {
        let Some(line) = new_lines.get((l.saturating_sub(1)) as usize) else {
            continue;
        };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(Some(old_line)) = unique.get(t) else {
            continue;
        };
        if let Some(h) = old_line_hash.get(old_line) {
            hits.push((h.clone(), l));
        }
    }
    Some(hits)
}

/// 结算侧入口：对本次提交"新增且无 AI 归属"的行，尝试从 reset --hard 孤儿链的
/// note 中按内容认领归属。best-effort：任何失败都不阻断主结算流程。
///
/// 调用时机：recover_attribution 之后、human 兜底之前（普通路径）；
/// note 继承块之后（amend 路径）。已被非 h_ 归属覆盖的行不动（不覆盖既有结论）。
pub(crate) fn claim_content_from_orphaned_chains(
    repo: &Repository,
    commit_sha: &str,
    parent_sha: &str,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    authorship_log: &mut AuthorshipLog,
) {
    if committed_hunks.is_empty() || parent_sha == "initial" {
        return;
    }
    // 快速路径：仓库从未发生过 reset --hard → 零开销返回（绝大多数场景）。
    let path = records_path(repo);
    if !path.exists() {
        return;
    }
    // 合并提交（双 parent）归属清零策略优先，不在其上做认领。
    let is_merge = repo
        .find_commit(commit_sha.to_string())
        .map(|c| c.parent_count().unwrap_or(0) > 1)
        .unwrap_or(false);
    if is_merge {
        return;
    }

    // 目标行 = committed hunks 中没有"非 h_ 归属"的行。
    // h_（KnownHuman）是"无证据兜底"，不算已归属——等待被更权威的 AI 账覆盖
    // （与 amend 继承路径的 covered 判定一致）。
    let mut covered: HashMap<&str, HashSet<u32>> = HashMap::new();
    for fa in &authorship_log.attestations {
        let set = covered.entry(fa.file_path.as_str()).or_default();
        for e in &fa.entries {
            if e.hash.starts_with("h_") {
                continue;
            }
            for r in &e.line_ranges {
                for l in r.expand() {
                    set.insert(l);
                }
            }
        }
    }
    let mut targets: HashMap<String, Vec<u32>> = HashMap::new();
    for (file, ranges) in committed_hunks {
        if crate::authorship::ignore::should_exclude_checkpoint_path(file) {
            continue;
        }
        let cov = covered.get(file.as_str());
        let mut lines: Vec<u32> = Vec::new();
        for r in ranges {
            for l in r.expand() {
                if cov.map(|s| s.contains(&l)).unwrap_or(false) {
                    continue;
                }
                lines.push(l);
            }
        }
        lines.sort_unstable();
        lines.dedup();
        if !lines.is_empty() {
            targets.insert(file.clone(), lines);
        }
    }
    if targets.is_empty() {
        return;
    }

    let records = load_records_from(&path);
    if records.is_empty() {
        return;
    }

    let paths: Vec<String> = targets.keys().cloned().collect();
    let mut candidates: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for rec in &records {
        for sha in orphan_candidates(repo, &rec.old_head, &rec.new_head, &paths) {
            if seen.insert(sha.clone()) {
                candidates.push(sha);
            }
        }
        if candidates.len() >= MAX_CANDIDATES_TOTAL {
            break;
        }
    }
    if candidates.is_empty() {
        return;
    }

    let mut note_cache: HashMap<String, Option<AuthorshipLog>> = HashMap::new();
    // (file, hash) → 认领行号；以及每个 hash 的来源候选（合并 metadata 引用用）。
    let mut claims: HashMap<(String, String), Vec<u32>> = HashMap::new();
    let mut claim_sources: HashMap<String, String> = HashMap::new();
    let mut claimed_by_file: HashMap<String, HashSet<u32>> = HashMap::new();

    for (file, target_lines) in &targets {
        let new_content = read_file_at_commit(repo, commit_sha, file).unwrap_or_default();
        if new_content.trim().is_empty() {
            continue;
        }

        // 新 → 旧逐候选累积认领：近者优先（or_insert），更旧候选补齐剩余行
        // （同文件可能跨多个旧提交被分批重做；任一候选命中即搬运其账）。
        for cand in &candidates {
            let claimed_set = claimed_by_file.get(file.as_str());
            let remaining: Vec<u32> = target_lines
                .iter()
                .copied()
                .filter(|l| !claimed_set.map(|s| s.contains(l)).unwrap_or(false))
                .collect();
            if remaining.is_empty() {
                break;
            }

            if !note_cache.contains_key(cand) {
                let loaded = crate::git::notes_api::read_authorship(repo, cand);
                note_cache.insert(cand.clone(), loaded);
            }
            let cand_log = note_cache.get(cand).and_then(|o| o.as_ref());
            let Some(cand_log) = cand_log else {
                continue;
            };
            let Some(fa) = cand_log.attestations.iter().find(|a| a.file_path == *file) else {
                continue;
            };
            let line_hash = build_line_hash(&fa.entries);
            if line_hash.is_empty() {
                continue;
            }
            // "活账"过滤：已是当前提交祖先的候选（其归属由正常结算链处理），
            // 不重复搬运。
            if is_ancestor_commit(repo, cand, commit_sha) {
                continue;
            }
            let old_content = read_file_at_commit(repo, cand, file).unwrap_or_default();
            if old_content.trim().is_empty() {
                continue;
            }
            let Some(hits) = match_claim_lines(&new_content, &old_content, &remaining, &line_hash)
            else {
                continue;
            };
            for (hash, line) in hits {
                if claimed_by_file
                    .entry(file.clone())
                    .or_default()
                    .insert(line)
                {
                    claims.entry((file.clone(), hash.clone())).or_default().push(line);
                    claim_sources.entry(hash).or_insert_with(|| cand.clone());
                }
            }
        }
    }

    if claims.is_empty() {
        tracing::debug!(
            commit = %commit_sha,
            candidates = candidates.len(),
            "orphaned-chain content-claim: no content matched"
        );
        return;
    }

    let mut file_count = 0usize;
    let mut line_count = 0usize;
    for ((file, hash), mut lines) in claims {
        lines.sort_unstable();
        lines.dedup();
        line_count += lines.len();
        let fa = authorship_log.get_or_create_file(&file);
        let ranges = LineRange::compress_lines(&lines);
        // 与 amend 继承路径同模式：先让同行上的 h_（无证据兜底）让位，
        // 再写入更权威的 AI 归属，避免同一行 h_ 与 s_ 双重覆盖。
        for e in fa.entries.iter_mut() {
            if e.hash.starts_with("h_") {
                e.remove_line_ranges(&ranges);
            }
        }
        fa.entries
            .retain(|e| !(e.hash.starts_with("h_") && e.line_ranges.is_empty()));
        if let Some(existing) = fa.entries.iter_mut().find(|e| e.hash == hash) {
            existing.line_ranges.extend(ranges);
        } else {
            fa.add_entry(AttestationEntry::new(hash, ranges));
        }
        file_count += 1;
    }

    // metadata 引用合并：把来源 note 中对应 prompts/sessions 记录补入，
    // 否则搬运的 hash 在新 note 里是悬空引用（上报/payload 解析不到会话记录）。
    for (hash, cand) in &claim_sources {
        let Some(Some(cand_log)) = note_cache.get(cand) else {
            continue;
        };
        if hash.starts_with("s_") {
            let key = hash.split("::").next().unwrap_or(hash.as_str());
            if let Some(rec) = cand_log.metadata.sessions.get(key) {
                authorship_log
                    .metadata
                    .sessions
                    .entry(key.to_string())
                    .or_insert_with(|| rec.clone());
            }
        } else if !hash.starts_with("h_") {
            if let Some(rec) = cand_log.metadata.prompts.get(hash) {
                authorship_log
                    .metadata
                    .prompts
                    .entry(hash.clone())
                    .or_insert_with(|| rec.clone());
            }
        }
    }

    tracing::info!(
        commit = %commit_sha,
        candidates = candidates.len(),
        files = file_count,
        lines = line_count,
        "orphaned-chain content-claim applied (reset --hard 换基后重做的内容找回 AI 归属)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_hash(pairs: &[(u32, &str)]) -> HashMap<u32, String> {
        pairs.iter().map(|(l, h)| (*l, h.to_string())).collect()
    }

    #[test]
    fn test_match_exact_ignoring_line_endings_maps_same_lines() {
        let old = "line1\r\nline2\r\nline3\r\n";
        let new = "line1\nline2\nline3\n";
        let lh = line_hash(&[(1, "s_a"), (2, "s_a"), (3, "s_a")]);
        let hits = match_claim_lines(new, old, &[1, 2, 3], &lh).unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|(h, _)| h == "s_a"));
    }

    #[test]
    fn test_match_exact_only_claims_target_lines() {
        let content = "a\nb\nc\n";
        let lh = line_hash(&[(1, "s_a"), (2, "s_a"), (3, "s_a")]);
        let hits = match_claim_lines(content, content, &[2], &lh).unwrap();
        assert_eq!(hits, vec![("s_a".to_string(), 2)]);
    }

    #[test]
    fn test_match_fuzzy_unique_only() {
        // 旧文件里 "dup" 出现多次 → 歧义不认；"unique_line" 唯一 → 认。
        let old = "keep1\nkeep2\ndup\ndup\nunique_line\nkeep3\n";
        let new = "keep1\nkeep2\ndup\ndup\nunique_line\nkeep3\nkeep4\n";
        let lh = line_hash(&[
            (3, "s_x"),
            (4, "s_x"),
            (5, "s_y"),
        ]);
        let hits = match_claim_lines(new, old, &[3, 4, 5, 7], &lh).unwrap();
        // 行 3/4（dup）歧义放弃；行 5（unique_line）命中；行 7 旧文件没有。
        assert_eq!(hits, vec![("s_y".to_string(), 5)]);
    }

    #[test]
    fn test_match_below_similarity_floor_returns_none() {
        let old = "alpha\nbeta\ngamma\n";
        let new = "totally\ndifferent\ncontent\n";
        let lh = line_hash(&[(1, "s_a")]);
        assert!(match_claim_lines(new, old, &[1, 2, 3], &lh).is_none());
    }

    #[test]
    fn test_match_exact_without_ai_attribution_returns_empty() {
        let content = "a\nb\n";
        // 旧文件行归属都是 h_ → build_line_hash 会排除，此处模拟空 map。
        let lh = HashMap::new();
        let hits = match_claim_lines(content, content, &[1, 2], &lh).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_build_line_hash_skips_human_entries() {
        let entries = vec![
            AttestationEntry::new("h_abc".to_string(), vec![LineRange::Range(1, 3)]),
            AttestationEntry::new("s_def".to_string(), vec![LineRange::Range(2, 5)]),
        ];
        let map = build_line_hash(&entries);
        assert!(!map.contains_key(&1));
        assert_eq!(map.get(&2).map(String::as_str), Some("s_def"));
        assert_eq!(map.get(&5).map(String::as_str), Some("s_def"));
    }

    #[test]
    fn test_records_roundtrip_prune_and_cap() {
        let dir = std::env::temp_dir().join(format!(
            "git-ai-orphan-test-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ORPHAN_CHAINS_FILENAME);

        // roundtrip
        let recs = vec![OrphanRecord {
            old_head: "a".repeat(40),
            new_head: "b".repeat(40),
            ts: now_secs(),
        }];
        save_records_to(&path, &recs);
        let loaded = load_records_from(&path);
        assert_eq!(loaded, recs);

        // 过期清理：ts=0 的记录读不出来
        let stale = vec![OrphanRecord {
            old_head: "c".repeat(40),
            new_head: "d".repeat(40),
            ts: 0,
        }];
        save_records_to(&path, &stale);
        assert!(load_records_from(&path).is_empty());

        // 上限：60 条只存最后 50 条
        let many: Vec<OrphanRecord> = (0..60)
            .map(|i| OrphanRecord {
                old_head: format!("{:040}", i),
                new_head: format!("{:040}", i + 1000),
                ts: now_secs(),
            })
            .collect();
        save_records_to(&path, &many);
        let loaded = load_records_from(&path);
        assert_eq!(loaded.len(), MAX_ORPHAN_RECORDS);
        assert_eq!(loaded.first().unwrap().old_head, format!("{:040}", 10));

        std::fs::remove_dir_all(&dir).ok();
    }
}
