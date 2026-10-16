//! Opt-in execution test in a disposable Linux environment with bubblewrap and
//! a root-owned, credential-free rootfs containing /bin/sh and coreutils/util-linux.
use anyhow::{Context, Result};
use codex_discord_bot::validation::{Check, Outcome, Policy, run_check};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "requires bubblewrap, user namespaces and TEST_VALIDATION_ROOTFS"]
async fn isolated_worker_runs_checks_without_host_credentials_or_escaping_descendants() -> Result<()>
{
    let rootfs = PathBuf::from(
        std::env::var("TEST_VALIDATION_ROOTFS").context("Set TEST_VALIDATION_ROOTFS")?,
    );
    let check = Check {
        argv: vec!["/usr/bin/true".into()],
        timeout_seconds: 5,
    };
    let policy = Policy {
        rootfs,
        max_repairs: 1,
        build: check.clone(),
        test: check.clone(),
        lint: check,
    };
    policy.verify_worker(&[])?;
    let worktree = tempfile::tempdir()?;
    let outside = tempfile::NamedTempFile::new()?;
    let check = Check { argv: vec!["/bin/sh".into(), "-c".into(),
        "test -z \"${DATABASE_URL+x}${DISCORD_TOKEN+x}${GH_TOKEN+x}${DOCKER_HOST+x}\" && test ! -e \"$1\" && test ! -e /run/docker.sock && test ! -e /var/run/docker.sock && ! grep -q validation_sand /proc/[0-9]*/comm && test \"$(wc -l < /proc/net/route)\" -eq 1 && test \"$PWD\" = /workspace && test \"$HOME\" = /tmp/home && ! touch /usr/worker-cannot-write && touch checked && echo isolated".into(),
        "check".into(), outside.path().to_str().unwrap().into()], timeout_seconds: 5 };
    let result = run_check(
        &policy,
        worktree.path(),
        "test",
        &check,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(result.outcome, Outcome::Passed, "{result:?}");
    assert_eq!(result.exit_code, Some(0));
    assert!(worktree.path().join("checked").exists());
    assert!(result.stdout.contains("isolated"));
    let failure = Check {
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo 'success'; exit 17".into(),
        ],
        timeout_seconds: 5,
    };
    let result = run_check(
        &policy,
        worktree.path(),
        "test",
        &failure,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(
        (result.outcome, result.exit_code),
        (Outcome::Failed, Some(17))
    );
    for cancelled in [false, true] {
        let cancel = CancellationToken::new();
        if cancelled {
            let token = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                token.cancel();
            });
        }
        let hung = Check {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo diagnostic; setsid /bin/sh -c 'sleep 2; touch escaped' & sleep 30".into(),
            ],
            timeout_seconds: 1,
        };
        let result = run_check(&policy, worktree.path(), "test", &hung, &cancel).await;
        assert_eq!(
            result.outcome,
            if cancelled {
                Outcome::Cancelled
            } else {
                Outcome::TimedOut
            },
            "{result:?}"
        );
        assert!(result.stdout.contains("diagnostic"));
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(!worktree.path().join("escaped").exists());
    }
    Ok(())
}
