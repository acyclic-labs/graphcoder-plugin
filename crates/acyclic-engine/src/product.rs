//! The public name and the identifiers derived from it. Every value here
//! comes from the repo-level `product.toml` through `build.rs`; nothing
//! user-facing may spell the name out.

/// The CLI command and the base of every derived path and identifier.
pub const NAME: &str = env!("PRODUCT_NAME");
/// Upper-case form for environment variables (`<PREFIX>_TRACE`, `<PREFIX>_HOOK`).
pub const ENV_PREFIX: &str = env!("PRODUCT_ENV_PREFIX");
/// Environment variable that turns on code-path tracing.
pub const TRACE_ENV: &str = concat!(env!("PRODUCT_ENV_PREFIX"), "_TRACE");
/// Environment variable that puts the CLI in hook mode.
pub const HOOK_ENV: &str = concat!(env!("PRODUCT_ENV_PREFIX"), "_HOOK");
/// The npm launcher package, for messages that point at the install path.
pub const NPM_PACKAGE: &str = env!("PRODUCT_NPM_PACKAGE");
/// `owner/repo` on GitHub, for release and issue links.
pub const GITHUB_REPO: &str = env!("PRODUCT_GITHUB_REPO");

/// The checked-in per-repo config directory, `.<name>`.
pub fn repo_config_dir() -> String {
    format!(".{NAME}")
}

/// The checked-in per-repo config file, `.<name>/config.toml`.
pub fn repo_config_file() -> String {
    format!(".{NAME}/config.toml")
}

/// Replaces `{{name}}` in a text template (skills, commands, AGENTS blocks).
pub fn render(template: &str) -> String {
    template.replace("{{name}}", NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_identifiers_follow_the_name() {
        assert!(!NAME.is_empty());
        assert_eq!(ENV_PREFIX, NAME.to_ascii_uppercase().replace('-', "_"));
        assert_eq!(TRACE_ENV, format!("{ENV_PREFIX}_TRACE"));
        assert_eq!(repo_config_file(), format!(".{NAME}/config.toml"));
        assert_eq!(render("run `{{name}} init` in .{{name}}"), format!("run `{NAME} init` in .{NAME}"));
    }
}
