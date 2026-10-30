//! `claude-sessions install DIR`: put this binary, and what Claude Code needs to call it, where
//! every dev container can read it.
//!
//! **The hub carries the one copy of the binary**, and its start writes it into a volume that
//! every dev container mounts read-only at the same path (infra's `dev/compose.yaml`). So the
//! version a container runs — its hooks, its agent, its offload pass, its status line — is
//! whatever the hub was built with, and a release reaches every container at the hub's next
//! start, with no image of theirs rebuilt. What lands in `DIR`:
//!
//! - `bin/claude-sessions` — this binary;
//! - `managed-settings.d/claude-sessions.json` — the hooks and the status line naming
//!   `DIR/bin/claude-sessions` (`hooks-config`), for a container to mount at
//!   `/etc/claude-code/managed-settings.d/`;
//! - `skills/<name>/SKILL.md` — the skills (`skill`): `keepalive`, and the three mail skills
//!   `mail-send`, `mail-read` and `mail-reply`, for a container's start to link into Claude
//!   Code's config directory. A file here it did not write — a skill infra keeps beside these —
//!   is left alone;
//! - `extensions/claude-sessions.js` — the pi extension (`pi-extension`), which feeds the
//!   same hook from a pi slot (`docs/design.md` § *Harnesses*), with `DIR/bin/claude-sessions`
//!   written in where it names the binary, for a container's start to link into pi's agent
//!   directory as it links the skills.
//!
//! **`DIR` must be the same path in the hub and in the containers**, because the drop-in names
//! the binary by it. Every file goes through a temporary name and a rename, so a container
//! reading it mid-install sees the old file or the new one, never half of one — and a running
//! hook keeps the binary it started from.

use crate::hooks_config;
use crate::json;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub fn run(dir: &Path) -> io::Result<Vec<String>> {
    if !dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not absolute — the hooks name the binary by this path, in every container",
                dir.display()
            ),
        ));
    }
    let bin = dir.join("bin/claude-sessions");
    let me = std::env::current_exe()?;
    let mut done = Vec::new();
    put(dir, &bin, &fs::read(&me)?, 0o755)?;
    done.push(bin.display().to_string());
    let settings = json::to_string_pretty(&hooks_config::settings(&bin.display().to_string()));
    let drop_in = dir.join("managed-settings.d/claude-sessions.json");
    put(dir, &drop_in, settings.as_bytes(), 0o644)?;
    done.push(drop_in.display().to_string());
    for (name, text) in crate::SKILLS {
        let skill = dir.join(format!("skills/{name}/SKILL.md"));
        put(dir, &skill, text.as_bytes(), 0o644)?;
        done.push(skill.display().to_string());
    }
    let ext = dir.join("extensions/claude-sessions.js");
    put(
        dir,
        &ext,
        pi_extension(&bin.display().to_string()).as_bytes(),
        0o644,
    )?;
    done.push(ext.display().to_string());
    Ok(done)
}

/// The pi extension, naming the binary at `exe`: `extensions/claude-sessions.js` as written,
/// with its placeholder filled. The path lands inside a JavaScript string literal, so a quote
/// or a backslash in it is escaped as JSON would — and a path with either is one nobody has.
pub fn pi_extension(exe: &str) -> String {
    let escaped = exe.replace('\\', "\\\\").replace('"', "\\\"");
    crate::PI_EXTENSION.replace("__CLAUDE_SESSIONS_BIN__", &escaped)
}

/// Write `bytes` to `path` whole: to a temporary name beside it, then renamed over it. The
/// directories from `root` down to it are made world-readable, as the files are: every
/// container reads them as its own `dev`, whatever its uid. **0644 for the drop-in is
/// load-bearing** — Claude Code reads an unreadable managed-settings file as no policy at
/// all, silently.
fn put(root: &Path, path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no directory"))?;
    fs::create_dir_all(parent)?;
    for d in parent.ancestors().take_while(|d| d.starts_with(root)) {
        fs::set_permissions(d, fs::Permissions::from_mode(0o755))?;
    }
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, bytes)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pi loads the skills too, and parses their frontmatter as strict YAML where Claude
    /// Code is lenient: an unquoted `description:` holding `: ` is a nested mapping to pi,
    /// which drops the skill and says so at start (infra, 2026-10-30, the first pi slot on
    /// the fleet). Without a YAML parser in the crate, this checks the rules such a value
    /// can break: a plain scalar carries no `: ` and no ` #` and starts with no indicator;
    /// a quoted one closes its quote and escapes any inside.
    #[test]
    fn every_skill_frontmatter_is_strict_yaml() {
        for (name, text) in crate::SKILLS.iter() {
            let mut lines = text.lines();
            assert_eq!(lines.next(), Some("---"), "{name}: frontmatter opens");
            for line in lines.by_ref().take_while(|l| *l != "---") {
                let (key, value) = line.split_once(": ").unwrap_or_else(|| {
                    panic!("{name}: frontmatter line `{line}` is not `key: value`")
                });
                assert!(
                    !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "{name}: key `{key}`"
                );
                assert!(
                    strict_scalar(value),
                    "{name}: `{key}:` value is not strict YAML: {value}"
                );
            }
        }
    }

    fn strict_scalar(v: &str) -> bool {
        match v.as_bytes().first() {
            Some(b'"') => {
                let inner = &v[1..];
                let mut escaped = false;
                let mut closed = None;
                for (i, c) in inner.char_indices() {
                    if closed.is_some() {
                        return false;
                    }
                    match (escaped, c) {
                        (true, _) => escaped = false,
                        (false, '\\') => escaped = true,
                        (false, '"') => closed = Some(i),
                        _ => {}
                    }
                }
                closed.is_some()
            }
            Some(b'\'') => {
                v.len() > 1
                    && v.ends_with('\'')
                    && !v[1..v.len() - 1].replace("''", "").contains('\'')
            }
            Some(b'[' | b'{' | b'&' | b'*' | b'!' | b'|' | b'>' | b'%' | b'@' | b'`' | b'#')
            | None => false,
            Some(_) => !v.contains(": ") && !v.contains(" #") && !v.ends_with(':'),
        }
    }

    #[test]
    fn install_writes_the_binary_the_drop_in_naming_it_and_the_skill_world_readable() {
        let dir = std::env::temp_dir().join(format!("cs-install-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(
            run(Path::new("relative")).is_err(),
            "a relative path names nothing"
        );
        let done = run(&dir).unwrap();
        assert_eq!(done.len(), 3 + crate::SKILLS.len());
        let bin = dir.join("bin/claude-sessions");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&bin), 0o755);
        assert_eq!(
            fs::read(&bin).unwrap(),
            fs::read(std::env::current_exe().unwrap()).unwrap()
        );
        let drop_in = dir.join("managed-settings.d/claude-sessions.json");
        assert_eq!(mode(&drop_in), 0o644);
        let text = fs::read_to_string(&drop_in).unwrap();
        assert!(
            text.contains(&format!("{} hook", bin.display())),
            "the hooks name the installed binary: {text}"
        );
        for (name, text) in crate::SKILLS {
            let path = dir.join(format!("skills/{name}/SKILL.md"));
            assert_eq!(fs::read_to_string(&path).unwrap(), text, "{name}");
            assert_eq!(mode(&path), 0o644, "{name}");
        }
        assert_eq!(
            fs::read_to_string(dir.join("skills/keepalive/SKILL.md")).unwrap(),
            crate::SKILL
        );
        let ext = fs::read_to_string(dir.join("extensions/claude-sessions.js")).unwrap();
        assert!(
            ext.contains(&format!("const BIN = \"{}\";", bin.display())),
            "the extension names the installed binary: {ext}"
        );
        assert!(!ext.contains("__CLAUDE_SESSIONS_BIN__"));
        assert_eq!(mode(&dir.join("extensions/claude-sessions.js")), 0o644);
        for d in [
            "",
            "bin",
            "managed-settings.d",
            "extensions",
            "skills",
            "skills/keepalive",
            "skills/mail-send",
            "skills/mail-read",
            "skills/mail-reply",
        ] {
            assert_eq!(mode(&dir.join(d)), 0o755, "{d}");
        }
        // Again, over itself, and nothing temporary is left behind — nor anything of somebody
        // else's taken away.
        let theirs = dir.join("skills/infra-only/SKILL.md");
        fs::create_dir_all(theirs.parent().unwrap()).unwrap();
        fs::write(&theirs, "infra's").unwrap();
        run(&dir).unwrap();
        assert_eq!(fs::read_to_string(&theirs).unwrap(), "infra's");
        let stray: Vec<_> = fs::read_dir(dir.join("bin"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
