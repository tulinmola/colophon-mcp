// Compiled into build.rs, which names the build, and into the tests that
// grade the naming.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// The kinds of file a build compiles. One that git ignores is compiled all the
// same, so it marks the build as an untracked one does.
const COMPILED: &[&str] = &[".c", ".h", ".rs"];

// A repository as a build found it: the commit it stood at, and whether the
// files the build is compiled from differ from that commit.
#[derive(Debug, PartialEq)]
pub struct Commit {
    pub id: String,
    pub dirty: bool,
}

// A hook, `rebase --exec` or an alias runs its commands with these set for the
// repository it was started in, and a git run under one would follow them
// there rather than to the repository it is pointed at; git names them itself.
fn repository_variables() -> &'static [String] {
    static VARIABLES: OnceLock<Vec<String>> = OnceLock::new();
    VARIABLES.get_or_init(|| {
        Command::new("git")
            .args(["rev-parse", "--local-env-vars"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    })
}

pub fn git_command() -> Command {
    let mut command = Command::new("git");
    for variable in repository_variables() {
        command.env_remove(variable);
    }
    command
}

fn git(repository: &Path, arguments: &[&str]) -> Option<String> {
    // Without --no-optional-locks, reading the status rewrites the index,
    // which the build watches.
    let output = git_command()
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    })
}

// A copy unpacked inside someone else's checkout is not that checkout, and
// must not be named for its commit or rebuilt by it.
fn is_a_checkout(repository: &Path) -> bool {
    let Some(top) = git(repository, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    matches!(
        (fs::canonicalize(top), fs::canonicalize(repository)),
        (Ok(top), Ok(repository)) if top == repository
    )
}

pub fn commit(repository: &Path, sources: &[&str]) -> Option<Commit> {
    if !is_a_checkout(repository) {
        return None;
    }
    let id = git(repository, &["rev-parse", "HEAD"])?;
    let mut status = vec![
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--ignored=matching",
        "--",
    ];
    status.extend(sources);
    let changes = git(repository, &status)?;
    let dirty = changes.lines().any(|line| {
        let path = line.trim_end_matches('"');
        !line.starts_with("!! ") || COMPILED.iter().any(|kind| path.ends_with(kind))
    });
    Some(Commit { id, dirty })
}

// What git rewrites when the commit a checkout stands at moves, or when what
// it has staged does, which the files themselves would not show: HEAD, for a
// checkout or a commit made detached; the index; the directory of branches,
// rather than a branch's own file, because packing deletes that file and the
// directories that held it and a commit writes them anew; and the directories
// a checkout keeps its refs in when it keeps them in tables instead: its own,
// and for a linked worktree, which keeps its HEAD in its own and its branches
// in another, the one it shares. A path that does not exist is left out: cargo would rerun the
// script on every build for it.
pub fn watched(repository: &Path) -> Vec<PathBuf> {
    if !is_a_checkout(repository) {
        return Vec::new();
    }
    let mut paths: Vec<PathBuf> = ["HEAD", "index", "refs/heads", "reftable"]
        .iter()
        .filter_map(|name| git(repository, &["rev-parse", "--git-path", name]))
        .map(|path| repository.join(path))
        .collect();
    if let Some(common) = git(repository, &["rev-parse", "--git-common-dir"]) {
        paths.push(repository.join(common).join("reftable"));
    }
    paths.retain(|path| path.exists());
    paths.sort();
    paths.dedup();
    paths
}

fn part(repository: &str, commit: Option<&Commit>) -> String {
    match commit {
        Some(Commit { id, dirty: true }) => format!("{repository}.{id}.dirty"),
        Some(Commit { id, dirty: false }) => format!("{repository}.{id}"),
        None => format!("{repository}.none"),
    }
}

// The commits are build metadata on the package's own version, which semantic
// versioning sets after a plus and leaves out of a version's precedence:
// https://semver.org/spec/v2.0.0.html#spec-item-10
pub fn name(version: &str, mcp: Option<&Commit>, emulator: Option<&Commit>) -> String {
    format!(
        "{version}+{}.{}",
        part("mcp", mcp),
        part("emulator", emulator)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::SystemTime;

    const SOURCES: &[&str] = &["src"];

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "colophon-mcp-build-name-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("a scratch directory");
            Scratch(path)
        }

        fn write(&self, file: &str, contents: &str) {
            let path = self.0.join(file);
            fs::create_dir_all(path.parent().expect("a file inside it")).expect("its directory");
            fs::write(path, contents).expect("the file is written");
        }

        // Run with no configuration but its own, so that nobody's signing,
        // hooks or excludes take part.
        fn git(&self, arguments: &[&str]) -> String {
            let output = git_command()
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args([
                    "-c",
                    "user.name=Colophon",
                    "-c",
                    "user.email=colophon@example.invalid",
                ])
                .arg("-C")
                .arg(&self.0)
                .args(arguments)
                .output()
                .expect("git runs");
            assert!(output.status.success(), "git {arguments:?}: {output:?}");
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned()
        }

        fn checkout() -> Scratch {
            let scratch = Scratch::new();
            scratch.git(&["init", "--quiet", "--initial-branch=main"]);
            scratch.commit_the_core();
            scratch
        }

        fn commit_the_core(&self) {
            self.write("src/core.c", "int core;\n");
            self.write("docs/core.md", "The core.\n");
            self.git(&["add", "."]);
            self.git(&["commit", "--quiet", "--message=The core"]);
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_clean_checkout_is_named_by_its_commit() {
        let checkout = Scratch::checkout();
        let id = checkout.git(&["rev-parse", "HEAD"]);
        assert_eq!(id.len(), 40);
        assert_eq!(
            commit(&checkout.0, SOURCES),
            Some(Commit { id, dirty: false })
        );
    }

    #[test]
    fn an_edited_source_marks_it() {
        let checkout = Scratch::checkout();
        checkout.write("src/core.c", "int core = 1;\n");
        assert!(commit(&checkout.0, SOURCES).expect("a commit").dirty);
    }

    #[test]
    fn an_untracked_source_marks_it() {
        let checkout = Scratch::checkout();
        // As anyone may have set it; the status read must not obey it.
        checkout.git(&["config", "status.showUntrackedFiles", "no"]);
        checkout.write("src/deep/new.c", "int new;\n");
        assert!(commit(&checkout.0, SOURCES).expect("a commit").dirty);
    }

    #[test]
    fn a_source_staged_out_of_the_commit_marks_it() {
        let checkout = Scratch::checkout();
        checkout.git(&["rm", "--quiet", "--cached", "src/core.c"]);
        assert!(commit(&checkout.0, SOURCES).expect("a commit").dirty);
    }

    #[test]
    fn a_change_beside_the_sources_leaves_it_unmarked() {
        let checkout = Scratch::checkout();
        checkout.write("docs/core.md", "The core, rewritten.\n");
        checkout.write("docs/new.md", "New.\n");
        assert!(!commit(&checkout.0, SOURCES).expect("a commit").dirty);
    }

    #[test]
    fn an_ignored_file_leaves_it_unmarked() {
        let checkout = Scratch::checkout();
        checkout.write(".gitignore", "*.o\n");
        checkout.git(&["add", ".gitignore"]);
        checkout.git(&["commit", "--quiet", "--message=Ignore objects"]);
        checkout.write("src/core.o", "object\n");
        assert!(!commit(&checkout.0, SOURCES).expect("a commit").dirty);
    }

    #[test]
    fn an_ignored_source_of_a_kind_the_build_compiles_marks_it() {
        let checkout = Scratch::checkout();
        checkout.write(".git/info/exclude", "probe.c\n");
        checkout.write("src/probe.c", "int probe;\n");
        assert!(commit(&checkout.0, SOURCES).expect("a commit").dirty);
    }

    #[test]
    fn git_is_not_sent_to_the_repository_a_hook_runs_in() {
        let command = git_command();
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_owned())
            .collect();
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            assert!(removed.iter().any(|name| name == variable), "{variable}");
        }
    }

    #[test]
    fn a_directory_that_is_no_checkout_has_no_commit() {
        let directory = Scratch::new();
        directory.write("src/core.c", "int core;\n");
        assert_eq!(commit(&directory.0, SOURCES), None);
    }

    #[test]
    fn a_copy_inside_another_checkout_has_no_commit() {
        let checkout = Scratch::checkout();
        checkout.write("copy/src/core.c", "int core;\n");
        assert_eq!(commit(&checkout.0.join("copy"), SOURCES), None);
        assert_eq!(watched(&checkout.0.join("copy")), Vec::<PathBuf>::new());
    }

    // Every file under the watched paths, with the time it was last written,
    // which is what cargo compares to decide whether to rerun the script.
    fn written(paths: &[PathBuf]) -> Vec<(PathBuf, Option<SystemTime>)> {
        let mut files = Vec::new();
        let mut pending = paths.to_vec();
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                for entry in fs::read_dir(&path).expect("the directory is read") {
                    pending.push(entry.expect("a directory entry").path());
                }
            } else {
                let time = fs::metadata(&path).and_then(|metadata| metadata.modified());
                files.push((path, time.ok()));
            }
        }
        files.sort();
        files
    }

    fn watched_files_change(checkout: &Scratch, step: &[&str]) {
        let paths = watched(&checkout.0);
        let before = written(&paths);
        checkout.git(step);
        assert_ne!(
            before,
            written(&paths),
            "git {step:?} touched nothing watched"
        );
    }

    #[test]
    fn every_move_of_the_commit_touches_what_the_build_watches() {
        let checkout = Scratch::checkout();
        // No reflog, so that nothing rests on one.
        checkout.git(&["config", "core.logAllRefUpdates", "false"]);
        fs::remove_dir_all(checkout.0.join(".git/logs")).expect("the reflog is removed");
        watched_files_change(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "--message=Two"],
        );
        checkout.git(&["pack-refs", "--all"]);
        watched_files_change(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "--message=Three"],
        );
        watched_files_change(&checkout, &["checkout", "--quiet", "-b", "topic/branch"]);
        watched_files_change(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "--message=Four"],
        );
        watched_files_change(&checkout, &["reset", "--quiet", "--hard", "HEAD~1"]);
        watched_files_change(&checkout, &["checkout", "--quiet", "--detach"]);
        watched_files_change(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "--message=Five"],
        );
        watched_files_change(&checkout, &["rm", "--quiet", "--cached", "src/core.c"]);
        checkout.git(&["reset", "--quiet"]);
        // A branch whose name nests it in directories, which packing deletes.
        watched_files_change(
            &checkout,
            &["checkout", "--quiet", "-b", "topic/deep/branch"],
        );
        watched_files_change(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "--message=Six"],
        );
        checkout.git(&["pack-refs", "--all"]);
        watched_files_change(&checkout, &["reset", "--quiet", "--soft", "HEAD~1"]);
        checkout.git(&["pack-refs", "--all"]);
        watched_files_change(
            &checkout,
            &["update-ref", "refs/heads/topic/deep/branch", "HEAD~1"],
        );
    }

    // Refs kept in tables, as git does when a repository is made with
    // --ref-format=reftable; a git too old to make one passes over this.
    #[test]
    fn every_move_of_the_commit_in_a_table_of_refs_touches_what_the_build_watches() {
        let checkout = Scratch::new();
        let made = git_command()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(&checkout.0)
            .args([
                "init",
                "--quiet",
                "--initial-branch=main",
                "--ref-format=reftable",
            ])
            .output()
            .expect("git runs");
        if !made.status.success() {
            eprintln!("this git keeps no refs in tables");
            return;
        }
        checkout.commit_the_core();
        watched_files_change(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "--message=Two"],
        );
        watched_files_change(&checkout, &["checkout", "--quiet", "-b", "topic/branch"]);
        watched_files_change(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "--message=Three"],
        );
        watched_files_change(&checkout, &["reset", "--quiet", "--soft", "HEAD~1"]);
    }

    #[test]
    fn the_name_carries_both_commits_and_their_marks() {
        let mcp = Commit {
            id: "a".repeat(40),
            dirty: true,
        };
        let emulator = Commit {
            id: "b".repeat(40),
            dirty: false,
        };
        assert_eq!(
            name("0.0.0", Some(&mcp), Some(&emulator)),
            format!(
                "0.0.0+mcp.{}.dirty.emulator.{}",
                "a".repeat(40),
                "b".repeat(40)
            )
        );
        assert_eq!(
            name("0.0.0", None, Some(&emulator)),
            format!("0.0.0+mcp.none.emulator.{}", "b".repeat(40))
        );
    }
}
