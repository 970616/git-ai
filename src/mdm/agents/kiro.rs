use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    binary_exists, generate_diff, is_git_ai_checkpoint_command, kiro_config_dir,
    normalize_windows_path_for_shell, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// Name of the custom agent this installer creates for Kiro CLI.
/// Kiro 2.x has no on-disk config for its built-in default agent, so hooks
/// must live on a custom agent; we create our own and (if unset) make it the
/// default so users don't have to pass `--agent` manually.
const KIRO_AGENT_NAME: &str = "git-ai";
const KIRO_CATCH_ALL_MATCHER: &str = "*";
const KIRO_PRE_TOOL_CMD: &str = "checkpoint kiro --hook-input stdin";
const KIRO_POST_TOOL_CMD: &str = "checkpoint kiro --hook-input stdin";

/// v3 全局钩子文件名(存放于 ~/.kiro/hooks/),触发名 PascalCase。
/// Kiro CLI 3.x(引擎 v3)读此文件,对所有 agent/workspace 生效;
/// 2.x 引擎(v2)不读,需加 --agent-engine v3 或指定 git-ai agent。
const KIRO_V3_HOOKS_FILE: &str = "git-ai.json";
const KIRO_V3_HOOKS_NAME: &str = "git-ai-checkpoint";
const KIRO_V3_TRIGGER_PRE: &str = "PreToolUse";
const KIRO_V3_TRIGGER_POST: &str = "PostToolUse";

pub struct KiroInstaller;

impl KiroInstaller {
    /// ~/.kiro/agents/git-ai.json — the custom agent carrying our hooks.
    fn agent_config_path() -> PathBuf {
        kiro_config_dir().join("agents").join(format!("{KIRO_AGENT_NAME}.json"))
    }

    /// ~/.kiro/settings/cli.json — where `chat.defaultAgent` lives.
    fn cli_settings_path() -> PathBuf {
        kiro_config_dir().join("settings").join("cli.json")
    }

    /// ~/.kiro/hooks/git-ai.json — v3 全局钩子文件(所有 workspace 生效)。
    fn v3_hooks_path() -> PathBuf {
        kiro_config_dir().join("hooks").join(KIRO_V3_HOOKS_FILE)
    }

    /// 生成 v3 全局钩子文件内容(version "v1" + 顶层 name + hooks 数组,
    /// 官方 `agent validate --path` 校验通过的最小合法结构)。
    fn v3_hooks_content(binary_path: &Path) -> Value {
        let command = Self::hook_command(binary_path, KIRO_PRE_TOOL_CMD);
        json!({
            "version": "v1",
            "name": KIRO_V3_HOOKS_NAME,
            "hooks": [
                {
                    "name": "git-ai-checkpoint-pre",
                    "trigger": KIRO_V3_TRIGGER_PRE,
                    "matcher": ".*",
                    "action": {"type": "command", "command": command},
                    "timeout": 60,
                },
                {
                    "name": "git-ai-checkpoint-post",
                    "trigger": KIRO_V3_TRIGGER_POST,
                    "matcher": ".*",
                    "action": {"type": "command", "command": command},
                    "timeout": 60,
                },
            ],
        })
    }

    /// 写入/更新 v3 全局钩子文件(幂等)。
    fn install_v3_hooks_file(
        binary_path: &Path,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let hook_path = Self::v3_hooks_path();
        if let Some(dir) = hook_path.parent() {
            fs::create_dir_all(dir)?;
        }
        let existing_content = if hook_path.exists() {
            fs::read_to_string(&hook_path)?
        } else {
            String::new()
        };
        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };
        let desired = Self::v3_hooks_content(binary_path);
        if existing == desired {
            return Ok(None);
        }
        let new_content = serde_json::to_string_pretty(&desired)?;
        let diff_output = generate_diff(&hook_path, &existing_content, &new_content);
        if !dry_run {
            write_atomic(&hook_path, new_content.as_bytes())?;
        }
        Ok(Some(diff_output))
    }

    /// 配置 Kiro 默认引擎为 v3(未设置时不覆盖用户已有值)。
    /// v3 引擎才读取 ~/.kiro/hooks/*.json 全局钩子(v2 只认 agent 内嵌)。
    fn ensure_agent_engine_v3(dry_run: bool) -> Result<Option<String>, GitAiError> {
        let settings_paths = [
            Self::cli_settings_path(),
            kiro_config_dir().join("settings.json"),
        ];
        let mut first_diff: Option<String> = None;
        for settings_path in &settings_paths {
            if let Some(dir) = settings_path.parent() {
                fs::create_dir_all(dir)?;
            }
            let existing_content = if settings_path.exists() {
                fs::read_to_string(settings_path)?
            } else {
                String::new()
            };
            let existing: Value = if existing_content.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&existing_content)?
            };

            let already_set = existing
                .get("chat")
                .and_then(|c| c.get("agentEngine"))
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if already_set {
                continue;
            }

            let mut merged = existing.clone();
            let chat = merged.get("chat").cloned().unwrap_or_else(|| json!({}));
            let mut chat = chat;
            if let Some(obj) = chat.as_object_mut() {
                obj.insert("agentEngine".to_string(), json!("v3"));
            }
            if let Some(obj) = merged.as_object_mut() {
                obj.insert("chat".to_string(), chat);
            }

            if existing == merged {
                continue;
            }
            let new_content = serde_json::to_string_pretty(&merged)?;
            let diff_output = generate_diff(settings_path, &existing_content, &new_content);
            if first_diff.is_none() {
                first_diff = Some(diff_output);
            }
            if !dry_run {
                write_atomic(settings_path, new_content.as_bytes())?;
            }
        }
        Ok(first_diff)
    }

    /// Build the shell command for a hook entry.
    /// Kiro 2.x hook config has no documented `env` field, so GIT_AI_DAEMON_HOME
    /// is injected as a command prefix (command entries run through a shell).
    fn hook_command(binary_path: &Path, subcommand: &str) -> String {
        let binary_str = normalize_windows_path_for_shell(binary_path);
        let base = format!("{} {}", binary_str, subcommand);
        match std::env::var("GIT_AI_DAEMON_HOME") {
            Ok(home) if !home.is_empty() => format!("GIT_AI_DAEMON_HOME={} {}", home, base),
            _ => base,
        }
    }

    /// Merge our hooks into an existing agent config value (preserving any
    /// user-set name/description/other fields), returning true if changed.
    fn merge_hooks_into(
        existing: &Value,
        pre_tool_cmd: &str,
        post_tool_cmd: &str,
    ) -> Value {
        let mut merged = existing.clone();
        let mut hooks = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));

        for (hook_type, desired_cmd) in &[("preToolUse", pre_tool_cmd), ("postToolUse", post_tool_cmd)]
        {
            let mut entries = hooks
                .get(*hook_type)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let exists = entries.iter().any(|e| {
                e.get("command")
                    .and_then(|c| c.as_str())
                    .map(is_git_ai_checkpoint_command)
                    .unwrap_or(false)
            });
            if !exists {
                entries.push(json!({
                    "command": desired_cmd,
                    "matcher": KIRO_CATCH_ALL_MATCHER,
                }));
            }
            if let Some(obj) = hooks.as_object_mut() {
                obj.insert(hook_type.to_string(), Value::Array(entries));
            }
        }

        if let Some(obj) = merged.as_object_mut() {
            obj.insert("hooks".to_string(), hooks);
            // Ensure the agent has the required identity fields.
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if name.is_none() || name.as_deref() == Some("") {
                obj.insert("name".to_string(), json!(KIRO_AGENT_NAME));
            }
            if obj.get("description").is_none() {
                obj.insert(
                    "description".to_string(),
                    json!("AI code authorship tracking (git-ai)"),
                );
            }
        }
        merged
    }

    fn install_hooks_at(
        agent_path: &Path,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(dir) = agent_path.parent() {
            fs::create_dir_all(dir)?;
        }

        let existing_content = if agent_path.exists() {
            fs::read_to_string(agent_path)?
        } else {
            String::new()
        };
        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };

        let pre_cmd = Self::hook_command(&params.binary_path, KIRO_PRE_TOOL_CMD);
        let post_cmd = Self::hook_command(&params.binary_path, KIRO_POST_TOOL_CMD);
        let merged = Self::merge_hooks_into(&existing, &pre_cmd, &post_cmd);

        if existing == merged {
            return Ok(None);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(agent_path, &existing_content, &new_content);
        if !dry_run {
            write_atomic(agent_path, new_content.as_bytes())?;
        }
        Ok(Some(diff_output))
    }

    /// Set `chat.defaultAgent = git-ai` only when no default is configured yet,
    /// so we never clobber a user's existing agent preference.
    fn ensure_default_agent(dry_run: bool) -> Result<Option<String>, GitAiError> {
        let settings_path = Self::cli_settings_path();
        if let Some(dir) = settings_path.parent() {
            fs::create_dir_all(dir)?;
        }
        let existing_content = if settings_path.exists() {
            fs::read_to_string(&settings_path)?
        } else {
            String::new()
        };
        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };

        let already_set = existing
            .get("chat")
            .and_then(|c| c.get("defaultAgent"))
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if already_set {
            return Ok(None);
        }

        let mut merged = existing.clone();
        let chat = merged.get_mut("chat").cloned().unwrap_or_else(|| json!({}));
        let mut chat = chat;
        if let Some(obj) = chat.as_object_mut() {
            obj.insert("defaultAgent".to_string(), json!(KIRO_AGENT_NAME));
        }
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("chat".to_string(), chat);
        }

        if existing == merged {
            return Ok(None);
        }
        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(&settings_path, &existing_content, &new_content);
        if !dry_run {
            write_atomic(&settings_path, new_content.as_bytes())?;
        }
        Ok(Some(diff_output))
    }

    fn uninstall_hooks_at(agent_path: &Path, dry_run: bool) -> Result<Option<String>, GitAiError> {
        if !agent_path.exists() {
            return Ok(None);
        }
        let existing_content = fs::read_to_string(agent_path)?;
        let existing: Value = serde_json::from_str(&existing_content)?;
        let mut merged = existing.clone();
        let Some(hooks) = merged.get_mut("hooks") else {
            return Ok(None);
        };
        let mut changed = false;
        for hook_type in &["preToolUse", "postToolUse"] {
            if let Some(entries) = hooks.get_mut(*hook_type).and_then(|v| v.as_array_mut()) {
                let before = entries.len();
                entries.retain(|e| {
                    e.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| !is_git_ai_checkpoint_command(cmd))
                        .unwrap_or(true)
                });
                if entries.len() != before {
                    changed = true;
                }
            }
        }
        if !changed {
            return Ok(None);
        }
        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(agent_path, &existing_content, &new_content);
        if !dry_run {
            write_atomic(agent_path, new_content.as_bytes())?;
        }
        Ok(Some(diff_output))
    }
}

impl HookInstaller for KiroInstaller {
    fn name(&self) -> &str {
        "Kiro"
    }

    fn id(&self) -> &str {
        "kiro"
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("kiro-cli") || binary_exists("kiro");
        let has_dotfiles = kiro_config_dir().exists();

        if !has_binary && !has_dotfiles {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let agent_path = Self::agent_config_path();
        if !agent_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let content = fs::read_to_string(&agent_path)?;
        let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
        let mut hooks_installed = existing
            .get("hooks")
            .and_then(|h| h.get("preToolUse"))
            .and_then(|v| v.as_array())
            .map(|entries| {
                entries.iter().any(|e| {
                    e.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_git_ai_checkpoint_command)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        // v3 全局钩子文件也是一种"已安装"形态(v2 agent 或 v3 hooks 任一命中)。
        let v3_path = Self::v3_hooks_path();
        if v3_path.exists() && fs::read_to_string(&v3_path).is_ok_and(|s| s.contains("checkpoint kiro")) {
            hooks_installed = true;
        }
        let hooks_up_to_date = hooks_installed;

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["kiro-cli", "kiro"]
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let agent_diff = Self::install_hooks_at(&Self::agent_config_path(), params, dry_run)?;
        let default_diff = Self::ensure_default_agent(dry_run)?;
        let v3_diff = Self::install_v3_hooks_file(&params.binary_path, dry_run)?;
        let engine_diff = Self::ensure_agent_engine_v3(dry_run)?;
        // Report the first change (agent diff takes precedence).
        Ok(agent_diff.or(default_diff).or(v3_diff).or(engine_diff))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let agent_diff = Self::uninstall_hooks_at(&Self::agent_config_path(), dry_run)?;
        // v3 全局钩子文件:内容是我们的则删除;用户改过则保留提示。
        let v3_path = Self::v3_hooks_path();
        let v3_diff = if v3_path.exists() {
            let content = fs::read_to_string(&v3_path).unwrap_or_default();
            if content.contains("checkpoint kiro") {
                let diff = generate_diff(&v3_path, &content, "");
                if !dry_run {
                    let _ = fs::remove_file(&v3_path);
                }
                Some(diff)
            } else {
                None
            }
        } else {
            None
        };
        Ok(agent_diff.or(v3_diff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.old {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let agent_path = temp_dir
            .path()
            .join(".kiro")
            .join("agents")
            .join("git-ai.json");
        (temp_dir, agent_path)
    }

    fn binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: binary_path(),
        }
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn s1_fresh_install_creates_agent_with_hooks() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let (_td, path) = setup_test_env();
        fs::remove_file(&path).ok();

        let diff = KiroInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_some(), "should produce a diff");

        let agent = read_json(&path);
        assert_eq!(agent.get("name").and_then(|v| v.as_str()).unwrap(), "git-ai");
        for hook_type in &["preToolUse", "postToolUse"] {
            let entries = agent
                .get("hooks")
                .and_then(|h| h.get(*hook_type))
                .and_then(|v| v.as_array())
                .unwrap();
            assert_eq!(entries.len(), 1, "{hook_type}: expected 1 hook");
            assert_eq!(
                entries[0].get("command").and_then(|c| c.as_str()).unwrap(),
                "/usr/local/bin/git-ai checkpoint kiro --hook-input stdin"
            );
            assert_eq!(
                entries[0].get("matcher").and_then(|c| c.as_str()).unwrap(),
                "*"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn s2_hook_command_includes_daemon_home_prefix() {
        let _guard = EnvGuard::set("GIT_AI_DAEMON_HOME", "/dev/shm/git-ai-daemon");
        let (_td, path) = setup_test_env();
        fs::remove_file(&path).ok();

        KiroInstaller::install_hooks_at(&path, &params(), false).unwrap();
        let agent = read_json(&path);
        let cmd = agent
            .get("hooks")
            .and_then(|h| h.get("preToolUse"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|e| e.get("command"))
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(
            cmd,
            "GIT_AI_DAEMON_HOME=/dev/shm/git-ai-daemon /usr/local/bin/git-ai checkpoint kiro --hook-input stdin"
        );
    }

    #[test]
    #[serial_test::serial]
    fn s3_idempotent_already_installed() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let (_td, path) = setup_test_env();
        let cmd = "/usr/local/bin/git-ai checkpoint kiro --hook-input stdin";
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "name": "git-ai",
                "description": "AI code authorship tracking (git-ai)",
                "hooks": {
                    "preToolUse": [{"command": cmd, "matcher": "*"}],
                    "postToolUse": [{"command": cmd, "matcher": "*"}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = KiroInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert!(diff.is_none(), "should be idempotent");
    }

    #[test]
    #[serial_test::serial]
    fn s4_preserves_user_agent_fields() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "name": "git-ai",
                "description": "user custom description",
                "model": "performance",
                "hooks": {
                    "agentSpawn": [{"command": "echo hi"}]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        KiroInstaller::install_hooks_at(&path, &params(), false).unwrap();
        let agent = read_json(&path);
        assert_eq!(
            agent.get("description").and_then(|v| v.as_str()).unwrap(),
            "user custom description"
        );
        assert_eq!(agent.get("model").and_then(|v| v.as_str()).unwrap(), "performance");
        // user's own hooks preserved
        let spawn = agent
            .get("hooks")
            .and_then(|h| h.get("agentSpawn"))
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(spawn.len(), 1);
    }

    #[test]
    #[serial_test::serial]
    fn s5_default_agent_not_overwritten_when_set() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".kiro").join("settings").join("cli.json");
        let _config_guard = EnvGuard::set("KIRO_CONFIG_DIR", temp_dir.path().join(".kiro").to_str().unwrap());
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({
                "chat": {"defaultAgent": "my-agent"}
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = KiroInstaller::ensure_default_agent(false).unwrap();
        assert!(diff.is_none(), "must not overwrite existing defaultAgent");

        let settings = read_json(&settings_path);
        assert_eq!(
            settings.get("chat").and_then(|c| c.get("defaultAgent")).and_then(|v| v.as_str()).unwrap(),
            "my-agent"
        );
    }

    #[test]
    #[serial_test::serial]
    fn s6_default_agent_set_when_unset() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join(".kiro").join("settings").join("cli.json");
        let _config_guard = EnvGuard::set("KIRO_CONFIG_DIR", temp_dir.path().join(".kiro").to_str().unwrap());

        let diff = KiroInstaller::ensure_default_agent(false).unwrap();
        assert!(diff.is_some(), "should set defaultAgent when absent");
        let settings = read_json(&settings_path);
        assert_eq!(
            settings.get("chat").and_then(|c| c.get("defaultAgent")).and_then(|v| v.as_str()).unwrap(),
            "git-ai"
        );
    }

    // ---- v3 全局钩子文件 / engine 设置 ----

    fn setup_kiro_config_dir() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        temp_dir
    }

    #[test]
    #[serial_test::serial]
    fn v1_fresh_install_creates_v3_hooks_file() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let temp_dir = setup_kiro_config_dir();
        let _config_guard = EnvGuard::set("KIRO_CONFIG_DIR", temp_dir.path().to_str().unwrap());

        let path = kiro_config_dir().join("hooks").join("git-ai.json");
        let diff = KiroInstaller::install_v3_hooks_file(&PathBuf::from("/usr/local/bin/git-ai"), false)
            .unwrap();
        assert!(diff.is_some());

        let hooks = read_json(&path);
        assert_eq!(hooks.get("version").and_then(|v| v.as_str()).unwrap(), "v1");
        assert_eq!(
            hooks.get("name").and_then(|v| v.as_str()).unwrap(),
            "git-ai-checkpoint"
        );
        let arr = hooks.get("hooks").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr.len(), 2);
        for h in arr {
            let cmd = h
                .get("action")
                .and_then(|a| a.get("command"))
                .and_then(|c| c.as_str())
                .unwrap();
            assert!(cmd.contains("checkpoint kiro"), "cmd={cmd}");
            assert_eq!(h.get("trigger").and_then(|v| v.as_str()).unwrap().ends_with("ToolUse"), true);
        }
    }

    #[test]
    #[serial_test::serial]
    fn v2_v3_hooks_file_idempotent() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let temp_dir = setup_kiro_config_dir();
        let _config_guard = EnvGuard::set("KIRO_CONFIG_DIR", temp_dir.path().to_str().unwrap());

        KiroInstaller::install_v3_hooks_file(&PathBuf::from("/usr/local/bin/git-ai"), false).unwrap();
        let diff = KiroInstaller::install_v3_hooks_file(&PathBuf::from("/usr/local/bin/git-ai"), false)
            .unwrap();
        assert!(diff.is_none(), "second run should be idempotent");
    }

    #[test]
    #[serial_test::serial]
    fn v3_engine_set_when_unset() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let temp_dir = setup_kiro_config_dir();
        let _config_guard = EnvGuard::set("KIRO_CONFIG_DIR", temp_dir.path().to_str().unwrap());

        let diff = KiroInstaller::ensure_agent_engine_v3(false).unwrap();
        assert!(diff.is_some());
        let settings_path = kiro_config_dir().join("settings").join("cli.json");
        let settings = read_json(&settings_path);
        assert_eq!(
            settings
                .get("chat")
                .and_then(|c| c.get("agentEngine"))
                .and_then(|v| v.as_str())
                .unwrap(),
            "v3"
        );
    }

    #[test]
    #[serial_test::serial]
    fn v4_engine_not_overwritten_when_set() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let temp_dir = setup_kiro_config_dir();
        let _config_guard = EnvGuard::set("KIRO_CONFIG_DIR", temp_dir.path().to_str().unwrap());

        let settings_path = kiro_config_dir().join("settings").join("cli.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({"chat": {"agentEngine": "v2"}})).unwrap(),
        )
        .unwrap();

        let diff = KiroInstaller::ensure_agent_engine_v3(false).unwrap();
        assert!(diff.is_none(), "must not overwrite user engine setting");
        let settings = read_json(&settings_path);
        assert_eq!(
            settings
                .get("chat")
                .and_then(|c| c.get("agentEngine"))
                .and_then(|v| v.as_str())
                .unwrap(),
            "v2"
        );
    }

    #[test]
    #[serial_test::serial]
    fn s7_uninstall_removes_git_ai_hooks() {
        let _guard = EnvGuard::remove("GIT_AI_DAEMON_HOME");
        let (_td, path) = setup_test_env();
        let cmd = "/usr/local/bin/git-ai checkpoint kiro --hook-input stdin";
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "name": "git-ai",
                "hooks": {
                    "preToolUse": [
                        {"command": cmd, "matcher": "*"},
                        {"command": "echo user-hook", "matcher": "fs_write"}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = KiroInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some());
        let agent = read_json(&path);
        let pre = agent
            .get("hooks")
            .and_then(|h| h.get("preToolUse"))
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(pre.len(), 1, "user hook must be preserved");
        assert_eq!(
            pre[0].get("command").and_then(|c| c.as_str()).unwrap(),
            "echo user-hook"
        );
    }
}
