//! Which changed files something on the host would run or obey without being asked to: a
//! build script at the next `cargo build`, an `.envrc` on entering the folder, a workflow on
//! the forge, instructions a coding agent reads. Ordinary source isn't listed: it runs when
//! you run it. This is a fixed list, by path and file mode, and it is short of complete on
//! purpose: it marks where to look first and never says the rest is safe. Names are matched
//! whatever their case: on a filesystem that folds case, `.ENVRC` is `.envrc`.

/// Why a file is marked: a few words that finish "this file is marked because…".
pub type Why = &'static str;

/// Files by their whole name.
const NAMES: &[(&str, Why)] = &[
    ("build.rs", "cargo runs it at every build"),
    ("Cargo.toml", "names build scripts, proc macros and dependencies"),
    ("Cargo.lock", "pins which dependency code is built"),
    ("rust-toolchain", "picks the toolchain rustup installs and runs"),
    ("rust-toolchain.toml", "picks the toolchain rustup installs and runs"),
    (".envrc", "direnv runs it on entering the folder"),
    ("flake.nix", "nix evaluates it for the dev shell"),
    ("flake.lock", "pins what the dev shell is built from"),
    ("shell.nix", "nix evaluates it for the dev shell"),
    ("default.nix", "nix evaluates it"),
    ("devbox.json", "devbox builds the shell from it"),
    ("Makefile", "make runs it"),
    ("makefile", "make runs it"),
    ("GNUmakefile", "make runs it"),
    ("justfile", "just runs it"),
    ("Justfile", "just runs it"),
    ("Taskfile.yml", "task runs it"),
    ("Taskfile.yaml", "task runs it"),
    ("CMakePresets.json", "cmake reads build commands from it"),
    ("settings.gradle.kts", "gradle runs it"),
    (".pnp.cjs", "node loads it before anything else"),
    (".pnp.loader.mjs", "node loads it before anything else"),
    ("CMakeLists.txt", "cmake runs it at configure"),
    ("meson.build", "meson runs it at configure"),
    ("configure", "run to configure the build"),
    ("build.gradle", "gradle runs it"),
    ("build.gradle.kts", "gradle runs it"),
    ("settings.gradle", "gradle runs it"),
    ("pom.xml", "maven plugins run from it"),
    ("Rakefile", "rake runs it"),
    ("Gemfile", "bundler evaluates it"),
    ("Gemfile.lock", "pins which gems are installed"),
    ("setup.py", "pip runs it at install"),
    ("setup.cfg", "configures the Python build"),
    ("pyproject.toml", "names the Python build backend and dependencies"),
    ("conftest.py", "pytest imports it"),
    ("noxfile.py", "nox runs it"),
    ("tox.ini", "tox runs its commands"),
    ("package.json", "npm scripts and dependencies"),
    ("package-lock.json", "pins which packages are installed"),
    ("yarn.lock", "pins which packages are installed"),
    ("pnpm-lock.yaml", "pins which packages are installed"),
    ("bun.lockb", "pins which packages are installed"),
    ("uv.lock", "pins which packages are installed"),
    ("poetry.lock", "pins which packages are installed"),
    ("go.mod", "names the Go dependencies"),
    ("go.sum", "pins the Go dependencies"),
    (".npmrc", "npm reads registries and scripts settings from it"),
    (".yarnrc.yml", "yarn reads plugins and registries from it"),
    ("Dockerfile", "docker runs it at build"),
    ("docker-compose.yml", "docker compose starts what it names"),
    ("compose.yaml", "docker compose starts what it names"),
    ("Procfile", "process managers start what it names"),
    (".tool-versions", "version managers install what it names"),
    ("mise.toml", "mise installs tools and runs tasks from it"),
    (".mise.toml", "mise installs tools and runs tasks from it"),
    ("lefthook.yml", "git hooks run from it"),
    (".pre-commit-config.yaml", "git hooks run from it"),
    (".gitattributes", "tells git which filters and diff drivers to use"),
    (".gitmodules", "names the submodule URLs git fetches"),
    (".gitlab-ci.yml", "CI runs it on the forge"),
    ("CLAUDE.md", "coding agents take instructions from it"),
    ("AGENTS.md", "coding agents take instructions from it"),
    ("GEMINI.md", "coding agents take instructions from it"),
    (".cursorrules", "coding agents take instructions from it"),
    (".mcp.json", "names MCP servers that agents start"),
];

/// Files under a directory of this name, at any depth.
const DIRS: &[(&str, Why)] = &[
    (".cargo", "cargo config: runners, linkers, flags"),
    (".github", "CI workflows and actions on the forge"),
    (".circleci", "CI runs it on the forge"),
    (".vscode", "editor tasks and settings"),
    (".idea", "editor run configurations"),
    (".zed", "editor tasks and settings"),
    (".claude", "Claude Code settings, hooks and skills"),
    (".cursor", "coding agents take rules from it"),
    (".codex", "coding agents take configuration from it"),
    (".husky", "git hooks run from it"),
    (".githooks", "git hooks run from it"),
    (".devcontainer", "the dev container is built from it"),
    ("xtask", "cargo xtask builds and runs it"),
];

const EXTENSIONS: &[(&str, Why)] = &[
    (".nix", "nix evaluates it"),
    (".sh", "a shell script"),
    (".bash", "a shell script"),
    (".zsh", "a shell script"),
    (".fish", "a shell script"),
    (".ps1", "a shell script"),
    (".bat", "a shell script"),
    (".cmd", "a shell script"),
    (".mk", "make includes it"),
];

/// Why the file at `path`, with `mode` after the change, is marked; `None` for most files.
pub fn classify(path: &str, mode: Option<&str>) -> Option<Why> {
    match mode {
        Some("160000") => return Some("a submodule: other code, fetched from a URL"),
        Some("120000") => return Some("a symlink: what reads it reads its target"),
        _ => {}
    }
    let mut parts = path.split('/').rev();
    let name = parts.next().unwrap_or(path);
    if let Some(&(_, why)) = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some(why);
    }
    if let Some(why) = parts.find_map(|dir| DIRS.iter().find(|(d, _)| d.eq_ignore_ascii_case(dir)).map(|&(_, why)| why)) {
        return Some(why);
    }
    let lower = name.to_ascii_lowercase();
    if let Some(&(_, why)) = EXTENSIONS.iter().find(|(e, _)| lower.ends_with(e) && lower.len() > e.len()) {
        return Some(why);
    }
    (mode == Some("100755")).then_some("marked executable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_the_host_runs_are_marked_and_source_is_not() {
        for path in ["build.rs", "crates/core/build.rs", ".ENVRC", ".Cargo/config.toml", "RUN.SH", ".envrc", "flake.nix", "nix/shell/dev.nix", ".github/workflows/ci.yml", ".cargo/config.toml", "sub/.vscode/tasks.json", "CLAUDE.md", ".claude/settings.json", "Cargo.toml", "scripts/run.sh", "xtask/src/main.rs"] {
            assert!(classify(path, None).is_some(), "{path} should be marked");
        }
        for path in ["src/main.rs", "README.md", "docs/build.rs.md", "src/envrc.rs", "github/notes.txt", "sh", ".sh"] {
            assert_eq!(classify(path, None), None, "{path} should not be marked");
        }
    }

    #[test]
    fn modes_mark_what_a_name_does_not() {
        assert!(classify("tools/gen", Some("100755")).is_some());
        assert!(classify("docs/link", Some("120000")).unwrap().contains("symlink"));
        assert!(classify("vendor/lib", Some("160000")).unwrap().contains("submodule"));
        assert_eq!(classify("tools/gen", Some("100644")), None);
    }
}
