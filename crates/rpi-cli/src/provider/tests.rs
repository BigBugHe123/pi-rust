use super::*;
use crate::config::test_support::env_lock;
struct TestEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    prev_key: Option<std::ffi::OsString>,
    prev_tok: Option<std::ffi::OsString>,
    prev_base: Option<std::ffi::OsString>,
    prev_openai_key: Option<std::ffi::OsString>,
    prev_dir: Option<std::ffi::OsString>,
    _tmp: tempfile::TempDir,
}
impl TestEnv {
    fn new() -> Self {
        let guard = env_lock().lock().unwrap();
        let prev_key = std::env::var_os(ANTHROPIC_API_KEY_ENV);
        let prev_tok = std::env::var_os(ANTHROPIC_AUTH_TOKEN_ENV);
        let prev_base = std::env::var_os(ANTHROPIC_BASE_URL_ENV);
        let prev_openai_key = std::env::var_os(OPENAI_API_KEY_ENV);
        let prev_dir = std::env::var_os(config::CONFIG_DIR_ENV);
        std::env::remove_var(ANTHROPIC_API_KEY_ENV);
        std::env::remove_var(ANTHROPIC_AUTH_TOKEN_ENV);
        std::env::remove_var(ANTHROPIC_BASE_URL_ENV);
        std::env::remove_var(OPENAI_API_KEY_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
        Self {
            _guard: guard,
            prev_key,
            prev_tok,
            prev_base,
            prev_openai_key,
            prev_dir,
            _tmp: tmp,
        }
    }
}
impl Drop for TestEnv {
    fn drop(&mut self) {
        restore(ANTHROPIC_API_KEY_ENV, self.prev_key.take());
        restore(ANTHROPIC_AUTH_TOKEN_ENV, self.prev_tok.take());
        restore(ANTHROPIC_BASE_URL_ENV, self.prev_base.take());
        restore(OPENAI_API_KEY_ENV, self.prev_openai_key.take());
        restore(config::CONFIG_DIR_ENV, self.prev_dir.take());
    }
}
fn restore(name: &str, prev: Option<std::ffi::OsString>) {
    match prev {
        Some(v) => std::env::set_var(name, v),
        None => std::env::remove_var(name),
    }
}

#[test]
fn trusted_project_defaults_override_global_defaults_for_resolution() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "global": {
      "api": "anthropic-messages",
      "apiKey": "global-secret",
      "models": [{"id":"global-model"}]
    },
    "project": {
      "api": "anthropic-messages",
      "apiKey": "project-secret",
      "models": [{"id":"project-model"}]
    }
  }
}"#,
    )
    .unwrap();
    std::fs::write(
        config::settings_path().unwrap(),
        r#"{"defaultProvider":"global","defaultModel":"global-model"}"#,
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".rpi")).unwrap();
    std::fs::write(
        project.path().join(".rpi/settings.json"),
        r#"{"defaultProvider":"project","defaultModel":"project-model"}"#,
    )
    .unwrap();

    let trusted = resolve_for_cwd(None, None, None, None, None, project.path(), true).unwrap();
    assert_eq!(trusted.provider.id(), "project");
    assert_eq!(trusted.model.id, "project-model");

    let untrusted = resolve_for_cwd(None, None, None, None, None, project.path(), false).unwrap();
    assert_eq!(untrusted.provider.id(), "global");
    assert_eq!(untrusted.model.id, "global-model");
}
