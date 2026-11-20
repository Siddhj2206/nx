//! What nx's background jobs couldn't do, read once for every command
//! that reports it: `nx doctor` lists it, `nx notify` sends it to the
//! session. The root services leave the records: `sync-failures.json`
//! (an image with no host tier), a generation's `skipped` (packages left
//! out for an image), `upgrade-failure.json` (the weekly tool upgrade).

use nx_core::{Deployments, State, select};

/// One problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// An image has no host tier: what the last `nx sync` couldn't resolve.
    NoHostTier {
        /// "the booted image", "the staged update"…
        image: String,
        digest: String,
        why: String,
    },
    /// An image's newest generation leaves packages out.
    LeftOut {
        image: String,
        packages: Vec<String>,
    },
    /// The weekly tool upgrade failed.
    UpgradeFailed(String),
}

/// What the records say now. With the deployments, every deployed image
/// is looked at; without (not bootc), the current generation.
pub fn problems(state: &State, deployments: Option<&Deployments>) -> Vec<Problem> {
    let mut out = Vec::new();
    let failures = select::failures(state);
    for (digest, why) in &failures {
        out.push(Problem::NoHostTier {
            image: role(deployments, digest),
            digest: digest.clone(),
            why: why.clone(),
        });
    }
    let newest: Vec<(String, Option<u32>)> = match deployments {
        Some(d) => {
            let mut digests: Vec<String> = Vec::new();
            for digest in d.all().iter().filter_map(|d| d.image_digest.clone()) {
                if !failures.contains_key(&digest) && !digests.contains(&digest) {
                    digests.push(digest);
                }
            }
            digests
                .iter()
                .map(|digest| {
                    let n = select::newest_for(state, digest).ok().flatten();
                    (role(deployments, digest), n)
                })
                .collect()
        }
        None => vec![(
            "this machine".to_owned(),
            state.current_number().ok().flatten(),
        )],
    };
    for (image, n) in newest {
        let packages = n
            .and_then(|n| select::read_lock(state, n).ok())
            .map(|l| l.skipped)
            .unwrap_or_default();
        if !packages.is_empty() {
            out.push(Problem::LeftOut { image, packages });
        }
    }
    if let Some(why) = select::upgrade_failure(state) {
        out.push(Problem::UpgradeFailed(why));
    }
    out
}

/// Which deployment `digest` is, in words.
fn role(deployments: Option<&Deployments>, digest: &str) -> String {
    let Some(d) = deployments else {
        return "an image".to_owned();
    };
    let is = |dep: Option<&nx_core::Deployment>| {
        dep.and_then(|d| d.image_digest.as_deref()) == Some(digest)
    };
    if is(d.booted()) {
        "the booted image"
    } else if is(d.staged()) {
        "the staged update"
    } else if is(d.rollback()) {
        "the rollback image"
    } else {
        "an image"
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const STATUS: &str = include_str!("../../nx-core/tests/fixtures/rpm-ostree-status.json");

    #[test]
    fn reads_every_record_once() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("nx-problems-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = State::new(&dir);
        std::fs::create_dir_all(state.generation_dir(1))?;
        let deployments = Deployments::from_json(STATUS)?;
        let digest = |d: Option<&nx_core::Deployment>| {
            d.and_then(|d| d.image_digest.clone()).unwrap_or_default()
        };
        let (booted, rollback) = (digest(deployments.booted()), digest(deployments.rollback()));
        let mut lock = nx_core::Lock::new(Some(booted), "45", "x86_64");
        lock.skipped = vec!["copr-thing".into()];
        std::fs::write(state.generation_dir(1).join("lock.toml"), lock.to_toml()?)?;
        select::record_failures(
            &state,
            &BTreeMap::from([(rollback.clone(), "no gcc".into())]),
        );
        select::record_upgrade_failure(&state, Some("no network"));
        let found = problems(&state, Some(&deployments));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            found,
            [
                Problem::NoHostTier {
                    image: "the rollback image".into(),
                    digest: rollback,
                    why: "no gcc".into()
                },
                Problem::LeftOut {
                    image: "the booted image".into(),
                    packages: vec!["copr-thing".into()]
                },
                Problem::UpgradeFailed("no network".into()),
            ]
        );
        Ok(())
    }
}
