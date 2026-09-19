//! Recovery has to run through the real CLI entry point before it can
//! canonicalize a repository name temporarily absent during Windows rewind.

#[cfg(windows)]
#[test]
fn cli_recovers_a_missing_repo_before_loading_config() -> Result<(), Box<dyn std::error::Error>> {
    for default_repo in [false, true] {
        let work = tempfile::tempdir()?;
        let parent = work.path().canonicalize()?;
        let repo = parent.join("repo");
        let scratch = parent.join(format!(".repo.{}-swap", acyclic::product::NAME));
        let staged = parent.join("staged");
        std::fs::create_dir(&scratch)?;
        std::fs::write(scratch.join("old.txt"), b"original")?;
        std::fs::create_dir(&staged)?;
        let store = parent.join("custom-store");
        let trash = store.join("trash");
        std::fs::create_dir_all(&trash)?;
        let journal = store.join("rewind-journal.json");
        std::fs::write(
            &journal,
            serde_json::json!({
                "target_generation": "00".repeat(32),
                "repo_root": repo,
                "tmp": staged,
                "phase": "Swapping",
                "carried": [],
            })
            .to_string(),
        )?;
        let locator = parent.join(format!(".repo.{}-rewind.json", acyclic::product::NAME));
        std::fs::write(
            &locator,
            serde_json::json!({
                "repo_root": repo,
                "journal": journal,
                "trash": trash,
                "trash_ttl_days": 30,
            })
            .to_string(),
        )?;

        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_acyclic"));
        command.env("HOME", &parent);
        command.current_dir(if default_repo { &scratch } else { &parent });
        if !default_repo {
            command.arg("--repo").arg("repo");
        }
        let output = command.arg("policy").output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(repo.join("old.txt"))?, b"original");
        assert!(!journal.exists());
        assert!(!locator.exists());
    }
    Ok(())
}
