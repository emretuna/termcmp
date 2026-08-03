use anyhow::{Context, Result};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub mod atomic_write;

use crate::sanitize::{sanitize_for_terminal, sanitize_path};

pub(crate) const ZSH_INTEGRATION: &str = include_str!("../../../shell/termcmp.zsh");
pub(crate) const ZSH_INIT: &str = include_str!("../../../shell/init.zsh");
pub(crate) const FISH_INTEGRATION: &str = include_str!("../../../shell/termcmp.fish");
pub(crate) const FISH_INIT: &str = include_str!("../../../shell/init.fish");
pub(crate) const BASH_INTEGRATION: &str = include_str!("../../../shell/termcmp.bash");

const DEFAULT_CONFIG_TOML: &str = "\
# Termcmp configuration
# Uncomment and edit values to customize. All values shown are defaults.

# [trigger]
# delay_ms = 150
# auto_trigger = true  # Set to false to disable all automatic triggers (manual keybinding only)

# [popup]
# max_visible = 10
# borders = false  # Set to true to enable rounded borders around the popup
# border_radius = true  # Set false for square corners (┌┐└┘) instead of rounded (╭╮╰╯)
# feedback_dismiss_ms = 1200  # Empty/error feedback auto-dismiss delay; 0 disables
# spinner = true  # Animate async Loading feedback in wide popups
# show_provider_errors = false  # Set true to show provider names in error feedback
# render_block_ms = 80  # Pre-paint window (ms, 0-300); 0 paints immediately, higher races fast async generators into the first frame
# min_width = 40  # Lower bound for popup width; clamped to [10, 500]
# max_width = 60  # Upper bound for popup width; clamped to [min_width, 500]
# description_box = \"off\"  # \"off\" for inline descriptions, \"side\" for wrapped adjacent box
# description_box_max_width = 60  # Max description-box width; clamped to [20, 200]
# description_box_lines = 5  # Max wrapped description lines; 0 resets to 5, above 20 clamps to 20
# description_box_debounce_ms = 80  # Description-box selection debounce; 0 disables
# tab_accepts_top = false  # Set true to make Tab accept the top suggestion when nothing is navigated (Fig/Kiro-style); Enter still runs the line
# index_hints = true  # Show selected/total index in the popup header
# key_hints = true  # Show keybinding hints in the popup footer
# nerd_icons = true  # Use Nerd Font glyphs for kind icons; set false for plain ASCII fallbacks

# [suggest]
# max_results = 50
# max_history_results = 5
# match_mode = \"fuzzy\"  # \"fuzzy\" = subsequence (gco -> git checkout); \"substring\" = contiguous (cl -> clone, not calendar)
# order = [\"ai\", \"history\", \"shell\", \"filesystem\", \"commands\", \"env\", \"ssh\"]  # Source-group ordering in the popup; earlier-listed sources appear first

# [suggest.providers]
# commands = true
# filesystem = true
# shell_completions = true  # Enable fish/zsh shell-native completion providers

# [keybindings]
# accept = \"tab\"
# accept_and_enter = \"enter\"
# dismiss = \"escape\"
# navigate_up = \"arrow_up\"
# navigate_down = \"arrow_down\"
# trigger = \"ctrl+/\"
# toggle_match_mode = \"ctrl+r\"  # Toggle match mode (fuzzy ↔ substring) while the popup is visible

[theme]
# name = \"dark\"  # Built-in: dark, light, catppuccin, material-darker, gruvbox, nord, dracula, tokyo-night — or a custom themes/<name>.toml file
# transparency = false  # Clear popup backgrounds so the terminal background shows through

# [ai.completion]
# enabled = false  # Enable LLM-powered inline completions
# provider = \"\"  # Provider name (key in [ai.providers])
# model = \"\"  # Model ID
# timeout_ms = 2000  # LLM request timeout (200-30000)
# max_results = 3  # Maximum LLM suggestions (1-10)
# max_tokens = 256  # Maximum tokens for LLM response (16-4096)
# thinking = \"off\"  # Thinking toggle for reasoning models: \"on\" | \"off\" | \"auto\"
#
# [ai.ask]
# enabled = false  # Show an on-demand \"Ask AI\" item at the top of the popup; selecting it asks the LLM and fills the prompt
# provider = \"\"  # Provider name (key in [ai.providers])
# model = \"\"  # Model ID
# timeout_ms = 15000  # LLM request timeout (200-30000); on-demand tolerates longer
# max_tokens = 512  # Maximum tokens for LLM response (16-4096)
# thinking = \"off\"  # Thinking toggle for reasoning models: \"on\" | \"off\" | \"auto\"
# To override the built-in AI completion prompt, create ~/.config/termcmp/prompt.md
# with your custom system prompt (raw text, entire file contents are used).
#
# WARNING: AI responses may be faulty or unsafe. ALWAYS double-check the command
# before pressing Enter — accepting an \"Ask AI\" result only fills the prompt, it
# never runs the command for you.
#
# [ai.providers.example]
# base_url = \"https://api.example.com/v1\"
# api_key = \"OPENAI_API_KEY\"  # Env var name or literal key; empty = no auth header
# api = \"openai-chat\"  # \"openai-chat\" or \"openai-responses\"
# thinking_budget = 0  # 0 disables extended thinking
# [ai.providers.example.extra_body]  # Optional server-specific fields merged into the request body
# chat_template_kwargs = { enable_thinking = false }  # e.g. Qwen3 on llama.cpp
# [[ai.providers.example.models]]
# id = \"gpt-4o\"
# name = \"GPT-4o\"

# [experimental]
# multi_terminal = false  # Allow running in unsupported terminals (at your own risk)
";

pub(crate) const INIT_BEGIN: &str = "# >>> termcmp initialize >>>";
pub(crate) const INIT_END: &str = "# <<< termcmp initialize <<<";
pub(crate) const SHELL_BEGIN: &str = "# >>> termcmp shell integration >>>";
pub(crate) const SHELL_END: &str = "# <<< termcmp shell integration <<<";
const MANAGED_WARNING: &str = "# !! Contents within this block are managed by 'termcmp install' !!";

/// Single-quote a path for safe embedding in shell code.
/// Escapes embedded single quotes with the `'\''` idiom.
///
/// Also strips ASCII/C1 control characters (ESC, BEL, NUL, CSI, etc.) from
/// the path text before quoting. The resulting snippet is later printed to
/// the user's terminal by `print_shell_blocks`, so a `$HOME`/config-derived
/// path containing crafted control bytes would otherwise be evaluated by
/// the terminal — single-quoting does not neutralise terminal escapes, only
/// shell metacharacters. Single-quote escaping happens after sanitisation
/// so that a legitimate single quote embedded in the path is still handled
/// correctly by the `'\''` idiom.
fn shell_safe_path(path: &Path) -> String {
    let s = sanitize_for_terminal(&path.display().to_string());
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn init_block(script_path: &Path) -> String {
    let path = shell_safe_path(script_path);
    format!(
        "{INIT_BEGIN}\n\
         {MANAGED_WARNING}\n\
         if [[ -f {path} ]]; then\n  \
         builtin source {path}\n\
         else\n  \
         echo \"termcmp: init script missing: \"{path} >&2\n  \
         echo \"termcmp: run 'termcmp install' to restore it\" >&2\n\
         fi\n\
         {INIT_END}"
    )
}

fn shell_integration_block(script_path: &Path) -> String {
    format!(
        "{SHELL_BEGIN}\n\
         {MANAGED_WARNING}\n\
         source {}\n\
         {SHELL_END}",
        shell_safe_path(script_path)
    )
}

/// Strips a managed block delimited by `begin`..`end` markers from `content`.
/// Returns `(new_content, was_found)`.
fn remove_block(content: &str, begin: &str, end: &str) -> (String, bool) {
    let mut content = content.to_string();
    let mut found = false;

    while let Some(start_idx) = content.find(begin) {
        let Some(end_match) = content[start_idx..].find(end) else {
            break;
        };
        let end_idx = start_idx + end_match + end.len();

        let mut result = String::with_capacity(content.len());
        result.push_str(&content[..start_idx]);
        // Skip trailing newline after end marker if present
        let after = if content[end_idx..].starts_with('\n') {
            &content[end_idx + 1..]
        } else {
            &content[end_idx..]
        };
        result.push_str(after);

        content = result;
        found = true;
    }

    (content, found)
}

/// A shell rc file present at its default path; an install target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellTarget {
    Zsh(PathBuf),  // ~/.zshrc
    Bash(PathBuf), // ~/.bashrc
    Fish(PathBuf), // ~/.config/fish/config.fish
}

/// Detect install targets purely from the filesystem — `$SHELL` is
/// deliberately ignored so every shell the user actually has configured
/// gets wired (a fish user whose login shell is still zsh gets both).
fn detect_shell_targets(home: &Path) -> Vec<ShellTarget> {
    let mut targets = Vec::new();
    let zshrc = home.join(".zshrc");
    if zshrc.exists() {
        targets.push(ShellTarget::Zsh(zshrc));
    }
    let bashrc = home.join(".bashrc");
    if bashrc.exists() {
        targets.push(ShellTarget::Bash(bashrc));
    }
    let fish_config = home.join(".config").join("fish").join("config.fish");
    if fish_config.exists() {
        targets.push(ShellTarget::Fish(fish_config));
    }
    targets
}

/// Fish managed block that sources the init script (placed near the top
/// of `~/.config/fish/config.fish`).
fn fish_init_block(init_path: &Path) -> String {
    let path = sanitize_path(init_path);
    format!(
        "{INIT_BEGIN}\n\
         {MANAGED_WARNING}\n\
         if test -f {path}\n  \
         source {path}\n\
         else\n  \
         echo \"termcmp: init script missing: {path}\" >&2\n  \
         echo \"termcmp: run 'termcmp install' to restore it\" >&2\n\
         end\n\
         {INIT_END}"
    )
}

/// Fish managed block that sources the shell integration script (placed
/// near the bottom of `~/.config/fish/config.fish`).
fn fish_shell_integration_block(script_path: &Path) -> String {
    format!(
        "{SHELL_BEGIN}\n\
         {MANAGED_WARNING}\n\
         source {}\n\
         {SHELL_END}",
        sanitize_path(script_path)
    )
}

/// Create the `themes/` directory inside the config dir so users can
/// drop custom theme TOML files.
fn create_themes_dir(config_dir: &Path) -> Result<()> {
    let themes_dir = config_dir.join("themes");
    fs::create_dir_all(&themes_dir)
        .with_context(|| format!("failed to create {}", themes_dir.display()))?;
    Ok(())
}

fn print_shell_blocks(init_path: &Path, script_path: &Path) {
    let init = init_block(init_path);
    let shell = shell_integration_block(script_path);
    let indented_init = init.replace('\n', "\n    ");
    let indented_shell = shell.replace('\n', "\n    ");

    println!(
        "  \x1b[36m\u{2139}\x1b[0m  Add the following \x1b[1mNEAR THE TOP\x1b[0m of your shell config:\n"
    );
    println!("    \x1b[36m{indented_init}\x1b[0m\n");
    println!(
        "  \x1b[36m\u{2139}\x1b[0m  Add the following \x1b[1mNEAR THE BOTTOM\x1b[0m of your shell config:\n"
    );
    println!("    \x1b[36m{indented_shell}\x1b[0m\n");
}

fn post_install_summary(config_dir: &Path, updated_rcs: &[String], target_count: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    if updated_rcs.len() == target_count {
        writeln!(
            out,
            "\x1b[32m\u{2713}\x1b[0m  termcmp installed successfully!"
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "\x1b[33m\u{26a0}\x1b[0m  termcmp partially installed (manual shell config step required)"
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    writeln!(out, "\x1b[1mNext steps:\x1b[0m").unwrap();
    if let Some((first, rest)) = updated_rcs.split_first() {
        writeln!(
            out,
            "  1. Reload your shell:     \x1b[1msource {first}\x1b[0m"
        )
        .unwrap();
        for rc in rest {
            writeln!(out, "                            \x1b[1msource {rc}\x1b[0m").unwrap();
        }
    } else {
        writeln!(
            out,
            "  1. Restart your shell after pasting the blocks above."
        )
        .unwrap();
    }
    writeln!(
        out,
        "  2. Verify the install:    \x1b[1mtermcmp doctor\x1b[0m"
    )
    .unwrap();
    writeln!(
        out,
        "  3. Try it:                \x1b[1mcd /tmp && git\x1b[0m\x1b[2m  (then type a space)\x1b[0m"
    )
    .unwrap();
    writeln!(
        out,
        "  4. Manual trigger:        \x1b[1mCtrl+/\x1b[0m  (if the popup doesn't appear)"
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "\x1b[1mFiles installed:\x1b[0m").unwrap();
    writeln!(
        out,
        "  Config:  {}",
        sanitize_path(&config_dir.join("config.toml"))
    )
    .unwrap();
    writeln!(
        out,
        "  Themes:  {}/",
        sanitize_path(&config_dir.join("themes"))
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "Docs: https://github.com/EmreTuna/termcmp#readme").unwrap();

    out
}
fn install_to(targets: &[ShellTarget], config_dir: &Path, dry_run: bool) -> Result<()> {
    // 1. Write shell scripts (zsh, bash, and fish — always, regardless of
    //    which rc files exist, so every shell's scripts stay deployed).
    let shell_dir = config_dir.join("shell");
    let zsh_init_path = shell_dir.join("init.zsh");
    let zsh_script_path = shell_dir.join("termcmp.zsh");
    let bash_script_path = shell_dir.join("termcmp.bash");
    let fish_init_path = shell_dir.join("init.fish");
    let fish_script_path = shell_dir.join("termcmp.fish");

    if dry_run {
        println!(
            "  Would write zsh init script to {}",
            sanitize_path(&zsh_init_path)
        );
        println!(
            "  Would write zsh integration to {}",
            sanitize_path(&zsh_script_path)
        );
        println!(
            "  Would write bash integration to {}",
            sanitize_path(&bash_script_path)
        );
        println!(
            "  Would write fish init script to {}",
            sanitize_path(&fish_init_path)
        );
        println!(
            "  Would write fish integration to {}",
            sanitize_path(&fish_script_path)
        );
        println!(
            "  Would create themes directory at {}",
            sanitize_path(&config_dir.join("themes"))
        );
        let config_path = config_dir.join("config.toml");
        if !config_path.exists() {
            println!(
                "  Would write default config to {}",
                sanitize_path(&config_path)
            );
        } else {
            println!("  Config already exists at {}", sanitize_path(&config_path));
        }
        for target in targets {
            match target {
                ShellTarget::Zsh(rc) => {
                    println!("  Would update {}\n", sanitize_path(rc));
                    println!("  \x1b[36m\u{2139}\x1b[0m  The following would be added to your shell config:\n");
                    print_shell_blocks(&zsh_init_path, &zsh_script_path);
                }
                ShellTarget::Bash(rc) => {
                    println!("  Would update {}\n", sanitize_path(rc));
                    println!("  \x1b[36m\u{2139}\x1b[0m  The following would be added to your shell config:\n");
                    let block = shell_integration_block(&bash_script_path);
                    println!("    \x1b[36m{}\x1b[0m\n", block.replace('\n', "\n    "));
                }
                ShellTarget::Fish(rc) => {
                    println!("  Would update {}\n", sanitize_path(rc));
                    println!("  \x1b[36m\u{2139}\x1b[0m  The following would be added to your fish config:\n");
                    let init = fish_init_block(&fish_init_path);
                    let shell = fish_shell_integration_block(&fish_script_path);
                    println!("    \x1b[36m{}\x1b[0m\n", init.replace('\n', "\n    "));
                    println!("    \x1b[36m{}\x1b[0m\n", shell.replace('\n', "\n    "));
                }
            }
        }
        return Ok(());
    }

    fs::create_dir_all(&shell_dir)
        .with_context(|| format!("failed to create {}", shell_dir.display()))?;

    atomic_write::atomic_write_preserving_mode(&zsh_init_path, ZSH_INIT.as_bytes())
        .with_context(|| format!("failed to write {}", zsh_init_path.display()))?;
    println!(
        "  Wrote zsh init script to {}",
        sanitize_path(&zsh_init_path)
    );

    atomic_write::atomic_write_preserving_mode(&zsh_script_path, ZSH_INTEGRATION.as_bytes())
        .with_context(|| format!("failed to write {}", zsh_script_path.display()))?;
    println!(
        "  Wrote zsh integration to {}",
        sanitize_path(&zsh_script_path)
    );

    atomic_write::atomic_write_preserving_mode(&bash_script_path, BASH_INTEGRATION.as_bytes())
        .with_context(|| format!("failed to write {}", bash_script_path.display()))?;
    println!(
        "  Wrote bash integration to {}",
        sanitize_path(&bash_script_path)
    );

    atomic_write::atomic_write_preserving_mode(&fish_init_path, FISH_INIT.as_bytes())
        .with_context(|| format!("failed to write {}", fish_init_path.display()))?;
    println!(
        "  Wrote fish init script to {}",
        sanitize_path(&fish_init_path)
    );

    atomic_write::atomic_write_preserving_mode(&fish_script_path, FISH_INTEGRATION.as_bytes())
        .with_context(|| format!("failed to write {}", fish_script_path.display()))?;
    println!(
        "  Wrote fish integration to {}",
        sanitize_path(&fish_script_path)
    );

    // 1b. Create themes directory
    create_themes_dir(config_dir)?;
    println!(
        "  Created themes directory at {}",
        sanitize_path(&config_dir.join("themes"))
    );

    // 1c. Write default config.toml if one doesn't exist (never clobber).
    let config_path = config_dir.join("config.toml");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
    {
        Ok(mut file) => {
            file.write_all(DEFAULT_CONFIG_TOML.as_bytes())
                .with_context(|| format!("failed to write {}", config_path.display()))?;
            println!("  Wrote default config to {}", sanitize_path(&config_path));
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            println!("  Config already exists at {}", sanitize_path(&config_path));
        }
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("failed to create {}", config_path.display()));
        }
    }

    // 2. Update every detected shell config
    let mut updated: Vec<String> = Vec::new();
    for target in targets {
        let ok = match target {
            ShellTarget::Zsh(rc) => install_zsh_config(rc, &zsh_init_path, &zsh_script_path)?,
            ShellTarget::Bash(rc) => install_bash_config(rc, &bash_script_path)?,
            ShellTarget::Fish(rc) => install_fish_config(rc, &fish_init_path, &fish_script_path)?,
        };
        if ok {
            updated.push(sanitize_path(match target {
                ShellTarget::Zsh(rc) | ShellTarget::Bash(rc) | ShellTarget::Fish(rc) => rc,
            }));
        }
    }
    print!(
        "\n{}",
        post_install_summary(config_dir, &updated, targets.len())
    );
    Ok(())
}

/// One-time backup of a shell rc file before first modification.
/// `create_new` keeps the original pristine across reinstalls; source
/// permissions are preserved; an unwritable parent dir skips the backup
/// with a notice instead of failing the install.
fn backup_rc_file(rc_path: &Path) -> Result<()> {
    if !rc_path.exists() {
        return Ok(());
    }
    let backup = rc_path.with_extension("backup.termcmp");
    let src_perms = fs::metadata(rc_path)
        .with_context(|| format!("failed to stat {}", rc_path.display()))?
        .permissions();
    let rc_bytes =
        fs::read(rc_path).with_context(|| format!("failed to read {}", rc_path.display()))?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&backup)
    {
        Ok(mut file) => {
            file.write_all(&rc_bytes)
                .with_context(|| format!("failed to backup to {}", backup.display()))?;
            fs::set_permissions(&backup, src_perms)
                .with_context(|| format!("failed to set permissions on {}", backup.display()))?;
            println!(
                "  Backed up {} to {}",
                sanitize_path(rc_path),
                sanitize_path(&backup)
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            println!("  Backup already exists at {}", sanitize_path(&backup));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let parent_display = backup
                .parent()
                .map(sanitize_path)
                .unwrap_or_else(|| sanitize_path(&backup));
            println!("  Skipping backup (cannot write to {parent_display})");
        }
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("failed to backup to {}", backup.display()));
        }
    }
    Ok(())
}

/// Write managed blocks into the zsh config file (`.zshrc`).
fn install_zsh_config(zshrc_path: &Path, init_path: &Path, script_path: &Path) -> Result<bool> {
    let existing = if zshrc_path.exists() {
        fs::read_to_string(zshrc_path)
            .with_context(|| format!("failed to read {}", zshrc_path.display()))?
    } else {
        String::new()
    };

    backup_rc_file(zshrc_path)?;

    // Strip existing managed blocks (idempotent)
    let (content, _) = remove_block(&existing, INIT_BEGIN, INIT_END);
    let (content, _) = remove_block(&content, SHELL_BEGIN, SHELL_END);

    // Prepend init block, append shell integration block.
    let mut new_zshrc = String::new();
    new_zshrc.push_str(&init_block(init_path));
    new_zshrc.push('\n');
    if !content.is_empty() {
        new_zshrc.push_str(&content);
        if !content.ends_with('\n') {
            new_zshrc.push('\n');
        }
    }
    new_zshrc.push_str(&shell_integration_block(script_path));
    new_zshrc.push('\n');

    match atomic_write::atomic_write_preserving_mode(zshrc_path, new_zshrc.as_bytes()) {
        Ok(()) => {
            println!("  Updated {}", sanitize_path(zshrc_path));
            Ok(true)
        }
        Err(e) if atomic_write::is_permission_denied(&e) => {
            println!(
                "\n  \x1b[33m\u{26a0}  Could not write to {} (permission denied)\x1b[0m\n",
                sanitize_path(zshrc_path)
            );
            print_shell_blocks(init_path, script_path);
            Ok(false)
        }
        Err(e) => Err(e.context(format!("failed to write {}", zshrc_path.display()))),
    }
}

/// Write the managed shell-integration block into `.bashrc`. Bash has no
/// init script (no proxy auto-exec — users launch `termcmp` manually), so
/// only the integration block is appended at the bottom.
fn install_bash_config(bashrc_path: &Path, script_path: &Path) -> Result<bool> {
    let existing = if bashrc_path.exists() {
        fs::read_to_string(bashrc_path)
            .with_context(|| format!("failed to read {}", bashrc_path.display()))?
    } else {
        String::new()
    };

    backup_rc_file(bashrc_path)?;

    // Strip existing managed blocks (idempotent). The INIT strip is a
    // harmless no-op (bash gets no init block) but keeps the pattern uniform.
    let (content, _) = remove_block(&existing, INIT_BEGIN, INIT_END);
    let (content, _) = remove_block(&content, SHELL_BEGIN, SHELL_END);

    // User content first, integration block at the bottom.
    let mut new_bashrc = String::new();
    if !content.is_empty() {
        new_bashrc.push_str(&content);
        if !content.ends_with('\n') {
            new_bashrc.push('\n');
        }
    }
    new_bashrc.push_str(&shell_integration_block(script_path));
    new_bashrc.push('\n');

    atomic_write::atomic_write_preserving_mode(bashrc_path, new_bashrc.as_bytes())
        .with_context(|| format!("failed to write {}", bashrc_path.display()))?;
    println!("  Updated {}", sanitize_path(bashrc_path));
    Ok(true)
}

/// Write managed blocks into the fish config file (`config.fish`).
fn install_fish_config(
    fish_config_path: &Path,
    init_path: &Path,
    script_path: &Path,
) -> Result<bool> {
    let existing = if fish_config_path.exists() {
        fs::read_to_string(fish_config_path)
            .with_context(|| format!("failed to read {}", fish_config_path.display()))?
    } else {
        String::new()
    };

    backup_rc_file(fish_config_path)?;

    // Strip existing managed blocks (idempotent)
    let (content, _) = remove_block(&existing, INIT_BEGIN, INIT_END);
    let (content, _) = remove_block(&content, SHELL_BEGIN, SHELL_END);

    // Prepend init block, append shell integration block.
    let mut new_config = String::new();
    new_config.push_str(&fish_init_block(init_path));
    new_config.push('\n');
    if !content.is_empty() {
        new_config.push_str(&content);
        if !content.ends_with('\n') {
            new_config.push('\n');
        }
    }
    new_config.push_str(&fish_shell_integration_block(script_path));
    new_config.push('\n');

    atomic_write::atomic_write_preserving_mode(fish_config_path, new_config.as_bytes())
        .with_context(|| format!("failed to write {}", fish_config_path.display()))?;
    println!("  Updated {}", sanitize_path(fish_config_path));
    Ok(true)
}

fn uninstall_from(
    zshrc_path: &Path,
    bashrc_path: &Path,
    fish_config_path: &Path,
    config_dir: &Path,
) -> Result<()> {
    if zshrc_path.exists() {
        let content = fs::read_to_string(zshrc_path)
            .with_context(|| format!("failed to read {}", zshrc_path.display()))?;

        let (content, found_init) = remove_block(&content, INIT_BEGIN, INIT_END);
        let (content, found_shell) = remove_block(&content, SHELL_BEGIN, SHELL_END);

        if found_init || found_shell {
            atomic_write::atomic_write_preserving_mode(zshrc_path, content.as_bytes())
                .with_context(|| format!("failed to write {}", zshrc_path.display()))?;
            println!(
                "  Removed managed blocks from {}",
                sanitize_path(zshrc_path)
            );
        } else {
            println!("  No termcmp blocks found in {}", sanitize_path(zshrc_path));
        }
    } else {
        println!(
            "  {} does not exist, nothing to do",
            sanitize_path(zshrc_path)
        );
    }

    // 1b. Strip managed blocks from .bashrc
    if bashrc_path.exists() {
        let content = fs::read_to_string(bashrc_path)
            .with_context(|| format!("failed to read {}", bashrc_path.display()))?;

        let (content, found_init) = remove_block(&content, INIT_BEGIN, INIT_END);
        let (content, found_shell) = remove_block(&content, SHELL_BEGIN, SHELL_END);

        if found_init || found_shell {
            atomic_write::atomic_write_preserving_mode(bashrc_path, content.as_bytes())
                .with_context(|| format!("failed to write {}", bashrc_path.display()))?;
            println!(
                "  Removed managed blocks from {}",
                sanitize_path(bashrc_path)
            );
        } else {
            println!(
                "  No termcmp blocks found in {}",
                sanitize_path(bashrc_path)
            );
        }
    } else {
        println!(
            "  {} does not exist, nothing to do",
            sanitize_path(bashrc_path)
        );
    }

    // 1c. Strip managed blocks from fish config
    if fish_config_path.exists() {
        let content = fs::read_to_string(fish_config_path)
            .with_context(|| format!("failed to read {}", fish_config_path.display()))?;

        let (content, found_init) = remove_block(&content, INIT_BEGIN, INIT_END);
        let (content, found_shell) = remove_block(&content, SHELL_BEGIN, SHELL_END);

        if found_init || found_shell {
            atomic_write::atomic_write_preserving_mode(fish_config_path, content.as_bytes())
                .with_context(|| format!("failed to write {}", fish_config_path.display()))?;
            println!(
                "  Removed managed blocks from {}",
                sanitize_path(fish_config_path)
            );
        } else {
            println!(
                "  No termcmp blocks found in {}",
                sanitize_path(fish_config_path)
            );
        }
    } else {
        println!(
            "  {} does not exist, nothing to do",
            sanitize_path(fish_config_path)
        );
    }

    // 2. Remove shell integration scripts (all shells)
    for name in &[
        "init.zsh",
        "termcmp.zsh",
        "termcmp.bash",
        "init.fish",
        "termcmp.fish",
    ] {
        let script_path = config_dir.join("shell").join(name);
        if script_path.exists() {
            fs::remove_file(&script_path)
                .with_context(|| format!("failed to remove {}", script_path.display()))?;
            println!("  Removed {}", sanitize_path(&script_path));
        }
    }

    // 3. Clean up empty shell/ directory (best-effort)
    let shell_dir = config_dir.join("shell");
    if shell_dir.exists() {
        let _ = fs::remove_dir(&shell_dir); // only succeeds if empty
    }

    // 4. Note about retained files
    let has_config = config_dir.join("config.toml").exists();
    if has_config {
        eprintln!();
        eprintln!("  \x1b[33mNote:\x1b[0m The following files were retained:");
        eprintln!("    - {}", sanitize_path(&config_dir.join("config.toml")));
        eprintln!(
            "  To remove everything: rm -rf {}",
            sanitize_path(config_dir)
        );
    }

    println!("\ntermcmp uninstalled successfully!");
    Ok(())
}

pub fn run_install(dry_run: bool) -> Result<()> {
    // Guard: refuse to install as root — creates root-owned files that break normal user's shell
    // SAFETY: libc::getuid() has no preconditions on POSIX and cannot fail.
    // It performs a single read of the real user ID from the kernel and
    // returns it. No pointer safety, no FFI lifetime concerns, no error path.
    if unsafe { libc::getuid() } == 0 {
        anyhow::bail!(
            "refusing to install as root — this would create root-owned files in your \
             home directory that break shell startup. Run without sudo."
        );
    }

    let home = dirs::home_dir().context("could not determine home directory")?;
    let config_dir = config::config_dir().context("could not determine home directory")?;

    // Install into every rc file that exists at its default path.
    let targets = detect_shell_targets(&home);
    if targets.is_empty() {
        anyhow::bail!(
            "no supported shell config found at the default paths:\n  \
             {}\n  {}\n  {}\n\
             Create one (or launch your shell once) and re-run `termcmp install`.",
            sanitize_path(&home.join(".zshrc")),
            sanitize_path(&home.join(".bashrc")),
            sanitize_path(&home.join(".config").join("fish").join("config.fish")),
        );
    }

    if dry_run {
        println!("Dry run: termcmp install\n");
    } else {
        println!("Installing termcmp...\n");
    }
    install_to(&targets, &config_dir, dry_run)
}

pub fn run_uninstall() -> Result<()> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let config_dir = config::config_dir().context("could not determine home directory")?;
    let zshrc_path = home.join(".zshrc");
    let bashrc_path = home.join(".bashrc");
    let fish_config_path = home.join(".config").join("fish").join("config.fish");

    println!("Uninstalling termcmp...\n");
    uninstall_from(&zshrc_path, &bashrc_path, &fish_config_path, &config_dir)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_remove_block_basic() {
        let content =
            "before\n# >>> termcmp initialize >>>\nstuff\n# <<< termcmp initialize <<<\nafter\n";
        let (result, found) = remove_block(content, INIT_BEGIN, INIT_END);
        assert!(found);
        assert_eq!(result, "before\nafter\n");
        assert!(!result.contains("termcmp initialize"));
    }

    #[test]
    fn test_remove_block_not_found() {
        let content = "just some shell config\nexport FOO=bar\n";
        let (result, found) = remove_block(content, INIT_BEGIN, INIT_END);
        assert!(!found);
        assert_eq!(result, content);
    }

    #[test]
    fn test_remove_block_multiple_occurrences() {
        let content =
            format!("a\n{INIT_BEGIN}\nx\n{INIT_END}\nb\n{INIT_BEGIN}\ny\n{INIT_END}\nc\n");
        let (result, found) = remove_block(&content, INIT_BEGIN, INIT_END);
        assert!(found);
        assert_eq!(result, "a\nb\nc\n");
    }

    #[test]
    fn test_remove_block_unterminated() {
        let content = format!("before\n{INIT_BEGIN}\nstuff but no end\n");
        let (result, found) = remove_block(&content, INIT_BEGIN, INIT_END);
        assert!(!found);
        assert_eq!(result, content);
    }

    #[test]
    fn test_remove_block_at_file_start() {
        let content = format!("{INIT_BEGIN}\nstuff\n{INIT_END}\nafter\n");
        let (result, found) = remove_block(&content, INIT_BEGIN, INIT_END);
        assert!(found);
        assert_eq!(result, "after\n");
    }

    #[test]
    fn test_remove_block_at_file_end_no_trailing_newline() {
        let content = format!("before\n{INIT_BEGIN}\nstuff\n{INIT_END}");
        let (result, found) = remove_block(&content, INIT_BEGIN, INIT_END);
        assert!(found);
        assert_eq!(result, "before\n");
    }

    #[test]
    fn test_remove_block_preserves_surrounding_content() {
        let content = format!("line1\nline2\n{INIT_BEGIN}\nmanaged\n{INIT_END}\nline3\n");
        let (result, found) = remove_block(&content, INIT_BEGIN, INIT_END);
        assert!(found);
        assert_eq!(result, "line1\nline2\nline3\n");
    }

    #[test]
    fn test_init_block_content() {
        let path = Path::new("/some/path/init.zsh");
        let block = init_block(path);
        assert!(block.contains(INIT_BEGIN));
        assert!(block.contains(INIT_END));
        assert!(block.contains(MANAGED_WARNING));
        // Source line pointing to external script (single-quoted)
        assert!(block.contains("builtin source '/some/path/init.zsh'"));
        assert!(block.contains("-f '/some/path/init.zsh'"));
        // Missing-file warning (else branch)
        assert!(block.contains("termcmp: init script missing:"));
        assert!(block.contains("termcmp install"));
    }

    #[test]
    fn test_init_script_content() {
        // Verify the external init script has all the required detection logic
        let script = ZSH_INIT;
        assert!(script.contains("__termcmp_init()"));
        assert!(script.contains("unset -f __termcmp_init"));
        assert!(script.contains("exec termcmp"));
        assert!(script.contains("command -v termcmp"));

        // --- Structural validation: guards must be in the correct branch ---

        // Split on the else branch to get tmux vs non-tmux sections
        let tmux_marker = "if [[ -n \"$TMUX\" ]]; then";
        assert!(script.contains(tmux_marker), "missing tmux branch");
        let tmux_start = script.find(tmux_marker).unwrap();
        let else_marker = script[tmux_start..].find("\n  else\n").unwrap();
        let tmux_branch = &script[tmux_start..tmux_start + else_marker];
        let non_tmux_branch = &script[tmux_start + else_marker..];

        // tmux branch: PPID check (quoted) + TERMCMP_PANE check
        assert!(
            tmux_branch.contains("ps -o comm= -p \"$PPID\""),
            "tmux branch must have quoted PPID check"
        );
        assert!(
            tmux_branch.contains("TERMCMP_PANE"),
            "tmux branch must have TERMCMP_PANE subshell guard"
        );
        assert!(
            tmux_branch.contains("$TMUX_PANE"),
            "tmux branch must compare TERMCMP_PANE against TMUX_PANE"
        );
        // tmux branch must NOT use TERMCMP_ACTIVE as a guard
        assert!(
            !tmux_branch.contains("[[ -n \"$TERMCMP_ACTIVE\" ]] && return"),
            "tmux branch must not use TERMCMP_ACTIVE as recursion guard"
        );

        // non-tmux branch: TERMCMP_ACTIVE guard
        assert!(
            non_tmux_branch.contains("TERMCMP_ACTIVE"),
            "non-tmux branch must use TERMCMP_ACTIVE guard"
        );

        // tmux branch: detect outer terminal via env vars
        assert!(tmux_branch.contains("$GHOSTTY_RESOURCES_DIR"));
        assert!(tmux_branch.contains("$KITTY_WINDOW_ID"));
        assert!(tmux_branch.contains("$WEZTERM_UNIX_SOCKET"));
        assert!(tmux_branch.contains("$ALACRITTY_SOCKET"));
        assert!(tmux_branch.contains("$ITERM_SESSION_ID"));
        assert!(tmux_branch.contains("\"$TERM_PROGRAM\" == \"rio\""));
        assert!(tmux_branch.contains("\"$TERM_PROGRAM\" == \"otty\""));
        assert!(tmux_branch.contains("$ZED_TERM"));
        assert!(tmux_branch.contains("$VSCODE_IPC_HOOK_CLI"));

        // Direct terminal detection (non-tmux)
        assert!(non_tmux_branch.contains("case \"$TERM_PROGRAM\""));
        assert!(non_tmux_branch
            .contains("ghostty|otty|WezTerm|rio|iTerm.app|Apple_Terminal|zed|vscode)"));
        assert!(non_tmux_branch.contains("$ZED_TERM"));
        assert!(non_tmux_branch.contains("$VSCODE_IPC_HOOK_CLI"));

        // Ancestor-walk-based reset for inherited TERMCMP_ACTIVE so the
        // `code .` flow still wires the proxy into VSCode's integrated terminal
        // when the env var propagated from the launching shell — while still
        // honoring the guard for subshells whose $PPID is another shell.
        assert!(
            non_tmux_branch.contains("_tc_ancestor_is_proxy"),
            "non-tmux branch must walk PPID ancestry to distinguish subshell \
             from leaked env var"
        );
        assert!(
            non_tmux_branch.contains("unset TERMCMP_ACTIVE"),
            "non-tmux branch must reset inherited TERMCMP_ACTIVE when \
             ancestry walk confirms leak"
        );
        // Both branches must coexist: honor the guard when the walk says
        // "descendant" (0) or "uncertain/ps failed" (2), and reset only when
        // the walk confirms the var is leaked (1).
        assert!(
            non_tmux_branch.matches("return").count() >= 2,
            "non-tmux branch must honor the guard on both 'descendant' and \
             'ps uncertain' outcomes"
        );

        // Ensure the OSC 7 encoder cannot pass ';' through unencoded — vte would
        // split the OSC param list and silently truncate the CWD.
        assert!(
            !ZSH_INTEGRATION.contains(";=/-"),
            "_tc_urlencode_path allow-list must not include URI sub-delimiters; \
             see termcmp.zsh _tc_urlencode_path doc and \
             osc7_roundtrip_with_semicolon_in_path test"
        );
        assert!(
            ZSH_INTEGRATION.contains("[a-zA-Z0-9._~/-])"),
            "_tc_urlencode_path must use the strict allow-list"
        );
    }

    #[test]
    fn test_zsh_integration_native_osc133_helper() {
        assert!(
            ZSH_INTEGRATION.contains("_tc_native_osc133()"),
            "zsh integration must define _tc_native_osc133 helper"
        );
        assert!(ZSH_INTEGRATION.contains("ZED_TERM"));
        assert!(ZSH_INTEGRATION.contains("VSCODE_INJECTION"));
        assert!(
            ZSH_INTEGRATION.contains("ghostty")
                || ZSH_INTEGRATION.contains("GHOSTTY_RESOURCES_DIR"),
            "helper must cover Ghostty"
        );
        assert!(
            ZSH_INTEGRATION.contains("_tc_native_osc133 || printf '\\e]7771;A"),
            "_tc_precmd must suppress OSC 7771 when terminal parses OSC 133 natively"
        );
        assert!(
            ZSH_INTEGRATION.contains("_tc_native_osc133 || printf '\\e]7771;C"),
            "_tc_preexec must suppress OSC 7771 when terminal parses OSC 133 natively"
        );
        // New native-OSC-133 terminals — match PromptDetection::Osc133 in terminal.
        assert!(
            ZSH_INTEGRATION.contains("KITTY_WINDOW_ID") || ZSH_INTEGRATION.contains("kitty"),
            "_tc_native_osc133 must recognise Kitty"
        );
        assert!(
            ZSH_INTEGRATION.contains("WEZTERM_UNIX_SOCKET") || ZSH_INTEGRATION.contains("WezTerm"),
            "_tc_native_osc133 must recognise WezTerm"
        );
        assert!(
            ZSH_INTEGRATION.contains("\"rio\""),
            "_tc_native_osc133 must recognise Rio"
        );
    }

    #[test]
    fn report_buffer_is_gated_on_active() {
        // GC-private frames must not leak to terminals when the proxy is absent.
        let body = ZSH_INTEGRATION
            .split("_tc_report_buffer()")
            .nth(1)
            .expect("found _tc_report_buffer")
            .split("\n}\n")
            .next()
            .expect("found end brace");
        assert!(
            body.contains("TERMCMP_ACTIVE"),
            "_tc_report_buffer must check $TERMCMP_ACTIVE before emitting OSC 7772"
        );
    }

    #[test]
    fn report_env_is_gated_on_active() {
        // OSC 7773 carries an env snapshot (PATH, AWS_PROFILE, GITHUB_TOKEN, …).
        // A regression that dropped or inverted the gate would leak the frame
        // to a bare terminal — and unlike OSC 7772 (line buffer), an env frame
        // contains values the user reasonably expects never to be rendered
        // verbatim. Pin the gate the same way `report_buffer_is_gated_on_active`
        // pins the OSC 7772 gate, so a `[[ -z … ]] || return` typo or an
        // accidental gate removal fails this test loudly.
        let body = ZSH_INTEGRATION
            .split("_tc_report_env()")
            .nth(1)
            .expect("found _tc_report_env")
            .split("\n}\n")
            .next()
            .expect("found end brace");
        assert!(
            body.contains("TERMCMP_ACTIVE"),
            "_tc_report_env must check $TERMCMP_ACTIVE before emitting OSC 7773"
        );
    }

    #[test]
    fn test_zsh_integration_vscode_injection_not_ipc_hook() {
        // Extract just the _tc_native_osc133 helper body to avoid matching
        // the unrelated __termcmp_init block (which does check
        // VSCODE_IPC_HOOK_CLI). The semantic split: detection uses the IPC
        // hook (terminal), suppression uses INJECTION (shell integration).
        let start = ZSH_INTEGRATION
            .find("_tc_native_osc133()")
            .expect("helper must be defined");
        let after = &ZSH_INTEGRATION[start..];
        let end = after.find("\n}").expect("helper must have closing brace");
        let helper_body = &after[..end];

        assert!(
            helper_body.contains("VSCODE_INJECTION"),
            "suppression helper must check VSCODE_INJECTION (the shell-integration signal)"
        );
        assert!(
            !helper_body.contains("VSCODE_IPC_HOOK_CLI"),
            "suppression helper must NOT check VSCODE_IPC_HOOK_CLI — that env var is for \
             detection, not suppression. Confusing the two would silently disable OSC 7771 \
             for VSCode users who have the integrated terminal open but haven't enabled \
             VSCode's shell integration."
        );
    }

    #[test]
    fn test_shell_integration_block_content() {
        let path = Path::new("/some/path/termcmp.zsh");
        let block = shell_integration_block(path);
        assert!(block.contains(SHELL_BEGIN));
        assert!(block.contains(SHELL_END));
        assert!(block.contains(MANAGED_WARNING));
        assert!(block.contains("source '/some/path/termcmp.zsh'"));
    }

    #[test]
    fn test_shell_safe_path_escapes_metacharacters() {
        // Dollar sign — would trigger variable expansion in double quotes
        let path = Path::new("/home/$USER/config/init.zsh");
        assert_eq!(shell_safe_path(path), "'/home/$USER/config/init.zsh'");

        // Backtick — would trigger command substitution in double quotes
        let path = Path::new("/home/user`whoami`/init.zsh");
        assert_eq!(shell_safe_path(path), "'/home/user`whoami`/init.zsh'");

        // Double quote — would break double-quoted embedding
        let path = Path::new("/home/us\"er/init.zsh");
        assert_eq!(shell_safe_path(path), "'/home/us\"er/init.zsh'");

        // Single quote — must be escaped with '\'' idiom
        let path = Path::new("/home/o'brien/init.zsh");
        assert_eq!(shell_safe_path(path), r"'/home/o'\''brien/init.zsh'");

        // Combined metacharacters
        let path = Path::new("/home/$(`evil'cmd\")/init.zsh");
        assert_eq!(
            shell_safe_path(path),
            r#"'/home/$(`evil'\''cmd")/init.zsh'"#
        );

        // Space in path — must be single-quoted to prevent word splitting
        let path = Path::new("/home/my user/config/init.zsh");
        assert_eq!(shell_safe_path(path), "'/home/my user/config/init.zsh'");

        // Tab in path — control character is stripped to prevent terminal
        // escape-sequence smuggling via `print_shell_blocks`, which prints
        // the rendered snippet directly to stdout.
        let path = Path::new("/home/user\t/init.zsh");
        assert_eq!(shell_safe_path(path), "'/home/user/init.zsh'");

        // Newline in path — control character is stripped to prevent both
        // terminal escape-sequence smuggling and shell command injection
        // via `$'\nrm -rf ~'`-style exploits.
        let path = Path::new("/home/user\n/init.zsh");
        assert_eq!(shell_safe_path(path), "'/home/user/init.zsh'");
    }

    #[test]
    fn test_shell_safe_path_strips_control_bytes() {
        // ESC-based CSI sequence: must be stripped before it reaches the
        // user's terminal via `print_shell_blocks`. Single-quote shell
        // escaping does NOT neutralise terminal escapes.
        let path = Path::new("/home/alice/\x1b[31mevil\x1b[0m/init.zsh");
        let quoted = shell_safe_path(path);
        assert!(
            !quoted.contains('\x1b'),
            "ESC byte must be stripped from shell snippet, got: {quoted:?}"
        );
        assert_eq!(quoted, "'/home/alice/[31mevil[0m/init.zsh'");

        // BEL (bell) character: commonly terminates OSC sequences.
        let path = Path::new("/home/\x07bob/init.zsh");
        let quoted = shell_safe_path(path);
        assert!(!quoted.contains('\x07'));
        assert_eq!(quoted, "'/home/bob/init.zsh'");

        // Sanitisation happens before single-quote escaping, so a legitimate
        // apostrophe in the path is still escaped correctly afterwards.
        let path = Path::new("/home/o'brien/\x1b[31mx/init.zsh");
        let quoted = shell_safe_path(path);
        assert!(!quoted.contains('\x1b'));
        assert_eq!(quoted, r"'/home/o'\''brien/[31mx/init.zsh'");
    }

    #[test]
    fn test_print_shell_blocks_sanitizes_paths() {
        // End-to-end: a `$HOME`/config-derived path containing ESC bytes
        // must not appear verbatim in the snippet emitted by the install
        // blocks. Covers both `init_block` and `shell_integration_block`,
        // which are the two places `print_shell_blocks` prints.
        let init = Path::new("/home/\x1b[31mbad/init.zsh");
        let script = Path::new("/home/\x07evil/termcmp.zsh");

        let rendered_init = init_block(init);
        let rendered_shell = shell_integration_block(script);

        assert!(
            !rendered_init.contains('\x1b'),
            "init_block must strip ESC: {rendered_init:?}"
        );
        assert!(
            !rendered_shell.contains('\x07'),
            "shell_integration_block must strip BEL: {rendered_shell:?}"
        );
        // Printable surroundings remain (literal `[31m` / `bad` survive).
        assert!(rendered_init.contains("[31mbad/init.zsh"));
        assert!(rendered_shell.contains("evil/termcmp.zsh"));
    }

    #[test]
    fn test_init_block_metacharacters_safe() {
        let path = Path::new("/home/$(rm -rf /)/config/termcmp/init.zsh");
        let block = init_block(path);
        // Must be single-quoted — no shell expansion possible
        assert!(block.contains("'/home/$(rm -rf /)/config/termcmp/init.zsh'"));
        // Must NOT contain the path inside double quotes (would allow expansion)
        assert!(!block.contains("\"$(rm -rf /)\""));
        // The echo line must close double quotes BEFORE the single-quoted path
        assert!(
            block.contains(
                r#"echo "termcmp: init script missing: "'/home/$(rm -rf /)/config/termcmp/init.zsh'"#
            ),
            "echo line must not embed single-quoted path inside double quotes:\n{}",
            block
        );
    }

    #[test]
    fn test_install_creates_files() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        // .zshrc should exist with both blocks
        let content = fs::read_to_string(&zshrc).unwrap();
        assert!(content.contains(INIT_BEGIN));
        assert!(content.contains(INIT_END));
        assert!(content.contains(SHELL_BEGIN));
        assert!(content.contains(SHELL_END));
        // Init script should be written and sourced
        let init_script = config.join("shell/init.zsh");
        assert!(init_script.exists());
        let init_content = fs::read_to_string(&init_script).unwrap();
        assert_eq!(init_content, ZSH_INIT);
        let expected_init_source = format!("builtin source {}", shell_safe_path(&init_script));
        assert!(
            content.contains(&expected_init_source),
            "init source path mismatch: .zshrc does not contain '{}'",
            expected_init_source
        );

        // Zsh shell integration script should be written and sourced
        let script = config.join("shell/termcmp.zsh");
        assert!(script.exists());
        let script_content = fs::read_to_string(&script).unwrap();
        assert_eq!(script_content, ZSH_INTEGRATION);
        let expected_source = format!("source {}", shell_safe_path(&script));
        assert!(
            content.contains(&expected_source),
            "source path mismatch: .zshrc does not contain '{}'",
            expected_source
        );

        // All shell scripts are deployed regardless of detected targets.
        assert!(config.join("shell/init.fish").exists());
        assert!(config.join("shell/termcmp.fish").exists());
        let bash_script = config.join("shell/termcmp.bash");
        assert!(bash_script.exists());
        assert_eq!(fs::read_to_string(&bash_script).unwrap(), BASH_INTEGRATION);
    }

    #[test]
    fn test_install_no_existing_zshrc() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        // .zshrc doesn't exist yet
        assert!(!zshrc.exists());
        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        let content = fs::read_to_string(&zshrc).unwrap();
        assert!(content.contains(INIT_BEGIN));
        assert!(content.contains(SHELL_BEGIN));
    }

    #[test]
    fn test_install_preserves_existing_content() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        let existing = "export PATH=\"/usr/local/bin:$PATH\"\nalias ll='ls -la'\n";
        fs::write(&zshrc, existing).unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        let content = fs::read_to_string(&zshrc).unwrap();
        assert!(content.contains("export PATH=\"/usr/local/bin:$PATH\""));
        assert!(content.contains("alias ll='ls -la'"));
        assert!(content.contains(INIT_BEGIN));
        assert!(content.contains(SHELL_BEGIN));

        // Init block should be before user content
        let init_pos = content.find(INIT_BEGIN).unwrap();
        let user_pos = content.find("export PATH").unwrap();
        let shell_pos = content.find(SHELL_BEGIN).unwrap();
        assert!(init_pos < user_pos);
        assert!(user_pos < shell_pos);
    }

    #[test]
    fn install_preserves_user_blank_lines_around_managed_blocks() {
        let tmp = TempDir::new().unwrap();
        let zshrc = tmp.path().join(".zshrc");
        let user_content = "\n\n# top comment\n\nalias g=git\n\n\n# bottom comment\n\n";
        fs::write(&zshrc, user_content).unwrap();

        let cfg_dir = tmp.path().join(".config/termcmp");
        fs::create_dir_all(&cfg_dir).unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &cfg_dir, false).expect("install");

        let after = fs::read_to_string(&zshrc).unwrap();

        let after_init_end =
            after.find(INIT_END).expect("init end marker present") + INIT_END.len();
        let user_region =
            &after[after_init_end..after.find(SHELL_BEGIN).expect("shell begin marker present")];
        assert!(
            user_region.contains("# top comment")
                && user_region.contains("alias g=git")
                && user_region.contains("# bottom comment"),
            "user content survived in middle region:\n{user_region}",
        );

        // The user's leading and trailing blank lines must survive verbatim.
        // `.trim()` on user content destroyed them on first install AND on
        // every reinstall. Look for the original "\n\n# top comment" and
        // "# bottom comment\n\n" framing inside the middle region.
        assert!(
            user_region.contains("\n\n# top comment"),
            "leading blank line before user content lost:\n{user_region:?}",
        );
        assert!(
            user_region.contains("# bottom comment\n\n"),
            "trailing blank line after user content lost:\n{user_region:?}",
        );

        // Reinstall should not accumulate blank lines around the managed blocks.
        install_to(&[ShellTarget::Zsh(zshrc.clone())], &cfg_dir, false).expect("reinstall");
        let after2 = fs::read_to_string(&zshrc).unwrap();
        let triple_blank = after2.matches("\n\n\n").count();
        let original_triple = after.matches("\n\n\n").count();
        assert!(
            triple_blank <= original_triple + 1,
            "reinstall accumulated blank lines: original={original_triple}, after={triple_blank}",
        );
    }

    #[test]
    fn test_idempotency() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        let existing = "export FOO=bar\n";
        fs::write(&zshrc, existing).unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();
        let first = fs::read_to_string(&zshrc).unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();
        let second = fs::read_to_string(&zshrc).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn test_uninstall_removes_blocks() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let fish_config = dir.path().join("config.fish");
        let bashrc = dir.path().join(".bashrc");
        let config = dir.path().join("config");

        let existing = "export FOO=bar\n";
        fs::write(&zshrc, existing).unwrap();
        // Seed .bashrc with a managed shell-integration block plus user content
        let bash_existing = format!(
            "export USER_LINE=1\n{SHELL_BEGIN}\n{MANAGED_WARNING}\nsource '/x/termcmp.bash'\n{SHELL_END}\n"
        );
        fs::write(&bashrc, &bash_existing).unwrap();

        // Install then uninstall
        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();
        uninstall_from(&zshrc, &bashrc, &fish_config, &config).unwrap();

        // Blocks should be gone
        let content = fs::read_to_string(&zshrc).unwrap();
        assert!(!content.contains(INIT_BEGIN));
        assert!(!content.contains(SHELL_BEGIN));
        assert!(content.contains("export FOO=bar"));

        // .bashrc managed block removed, user content survives
        let bash_content = fs::read_to_string(&bashrc).unwrap();
        assert!(!bash_content.contains(SHELL_BEGIN));
        assert!(bash_content.contains("export USER_LINE=1"));

        // Shell scripts should be removed
        assert!(!config.join("shell/init.zsh").exists());
        assert!(!config.join("shell/termcmp.zsh").exists());
        assert!(!config.join("shell/init.fish").exists());
        assert!(!config.join("shell/termcmp.fish").exists());
    }

    #[test]
    fn test_install_creates_backup() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        let existing = "export ORIGINAL=true\n";
        fs::write(&zshrc, existing).unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        // with_extension replaces .zshrc extension
        let backup = zshrc.with_extension("backup.termcmp");
        let backup_content = fs::read_to_string(&backup).unwrap();
        assert_eq!(backup_content, existing);
    }

    #[test]
    fn test_install_backup_preserves_source_mode() {
        // Regression for the install.rs TOCTOU fix: when fs::copy was replaced
        // with OpenOptions::create_new(true), mode preservation was silently
        // dropped and a 0o600 source .zshrc would be backed up as 0o644 (or
        // whatever the umask left), exposing shell secrets to other users.
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        fs::write(&zshrc, "export SECRET_TOKEN=hunter2\n").unwrap();
        // Restrict to owner-only, like a security-conscious user might.
        fs::set_permissions(&zshrc, fs::Permissions::from_mode(0o600)).unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        let backup = zshrc.with_extension("backup.termcmp");
        let backup_mode = fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            backup_mode, 0o600,
            "backup must preserve source mode — got {backup_mode:o}, expected 600"
        );
    }

    #[test]
    fn test_install_creates_default_config() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        let config_path = config.join("config.toml");
        assert!(config_path.exists());
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("[keybindings]"));
        assert!(content.contains("[trigger]"));
        assert!(content.contains("[popup]"));
        assert!(content.contains("[theme]"));
        assert!(content.contains("# min_width = 40"));
        assert!(content.contains("# max_width = 60"));
        assert!(content.contains("# description_box = \"off\""));
        assert!(content.contains("# description_box_max_width = 60"));
        assert!(content.contains("# description_box_lines = 5"));
        assert!(content.contains("# description_box_debounce_ms = 80"));
        // Should parse as valid TOML config (all theme fields are commented out)
        let parsed: config::TermcmpConfig = toml::from_str(&content).unwrap();
        assert_eq!(parsed.keybindings.accept, "tab");
    }

    #[test]
    fn test_install_does_not_clobber_existing_config() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        fs::create_dir_all(&config).unwrap();
        let config_path = config.join("config.toml");
        let custom = "[keybindings]\naccept = \"enter\"\n";
        fs::write(&config_path, custom).unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        let content = fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, custom);
    }

    #[test]
    fn test_install_creates_themes_dir() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        // The installer must create the themes/ directory so users can
        // drop custom theme TOML files without a manual mkdir step.
        assert!(config.join("themes").is_dir());
    }

    #[test]
    fn test_install_readonly_zshrc_succeeds() {
        use std::os::unix::fs::PermissionsExt;

        // Exercises the full first-install PermissionDenied path: parent
        // dir is read-only, so backup creation fails-fast with
        // PermissionDenied (tolerated, no backup made), then the .zshrc
        // atomic_write also hits PermissionDenied and the graceful
        // manual-instructions fallback runs. .zshrc must remain untouched
        // and the install must succeed.
        //
        // The atomic helper writes via `NamedTempFile::new_in(parent)` +
        // `rename(2)`, which on macOS succeeds even when only the target
        // file is read-only — the rename inherits the parent dir's perms,
        // not the target's. So we make .zshrc live in its own subdir and
        // chmod just that dir to 0o555, leaving config (which lives
        // elsewhere) writable.
        let dir = TempDir::new().unwrap();
        let zshrc_dir = dir.path().join("home");
        let config = dir.path().join("config");
        fs::create_dir_all(&zshrc_dir).unwrap();
        let zshrc = zshrc_dir.join(".zshrc");
        let original = "export FOO=bar\n";
        fs::write(&zshrc, original).unwrap();
        fs::set_permissions(&zshrc_dir, fs::Permissions::from_mode(0o555)).unwrap();

        // Install should succeed (graceful fallback, not error)
        let result = install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false);

        // Restore perms before any assertion-driven panic so TempDir
        // cleanup can recurse into zshrc_dir.
        fs::set_permissions(&zshrc_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            result.is_ok(),
            "install must take graceful fallback, got: {result:?}"
        );

        // File deployments to a writable directory should still have happened
        assert!(config.join("shell/init.zsh").exists());
        assert!(config.join("shell/termcmp.zsh").exists());
        assert!(config.join("themes").exists());

        // .zshrc content must be untouched — proof the fallback path ran
        // instead of silently rewriting the file. A regression that
        // simplified the chain walk would surface here as either an Err
        // (test fails on .is_ok()) or a modified .zshrc.
        assert_eq!(
            fs::read_to_string(&zshrc).unwrap(),
            original,
            "fallback path must leave .zshrc untouched"
        );

        // No backup should have been created — backup creation was
        // skipped (not silently succeeded) when the parent dir is
        // unwritable.
        assert!(
            !zshrc.with_extension("backup.termcmp").exists(),
            "no backup should be created when parent dir is unwritable"
        );
    }

    #[test]
    fn test_install_dry_run_no_writes() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, true).unwrap();

        // Nothing should have been created
        assert!(!zshrc.exists());
        assert!(!config.exists());
    }

    #[test]
    fn test_install_dry_run_existing_files_untouched() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        let existing = "export FOO=bar\n";
        fs::write(&zshrc, existing).unwrap();

        fs::create_dir_all(&config).unwrap();
        let config_path = config.join("config.toml");
        let custom_config = "[keybindings]\naccept = \"enter\"\n";
        fs::write(&config_path, custom_config).unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, true).unwrap();

        // .zshrc should be unchanged
        assert_eq!(fs::read_to_string(&zshrc).unwrap(), existing);
        // config should be unchanged
        assert_eq!(fs::read_to_string(&config_path).unwrap(), custom_config);
        // No shell scripts should have been created
        assert!(!config.join("shell").exists());
    }

    #[test]
    fn test_backup_not_overwritten_on_reinstall() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let config = dir.path().join("config");

        let original = "export ORIGINAL=true\n";
        fs::write(&zshrc, original).unwrap();

        // First install — creates backup with original content
        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();
        let backup = zshrc.with_extension("backup.termcmp");
        assert!(backup.exists());
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);

        // Second install — backup should NOT be overwritten
        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            original,
            "backup was overwritten on second install — original content lost"
        );
    }

    #[test]
    fn test_uninstall_prints_retained_files_note() {
        let dir = TempDir::new().unwrap();
        let zshrc = dir.path().join(".zshrc");
        let fish_config = dir.path().join("config.fish");
        let bashrc = dir.path().join(".bashrc");
        let config = dir.path().join("config");

        fs::write(&zshrc, "export FOO=bar\n").unwrap();
        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        // After install, themes and config should exist
        assert!(config.join("themes").exists());
        assert!(config.join("config.toml").exists());

        // Uninstall — should succeed and leave themes/config behind
        uninstall_from(&zshrc, &bashrc, &fish_config, &config).unwrap();

        // Themes and config should still be there (retained)
        assert!(config.join("themes").exists());
        assert!(config.join("config.toml").exists());
    }

    #[test]
    fn test_post_install_summary_contains_all_sections() {
        // Use a distinctive directory so we can also assert the helper
        // interpolates `config_dir` into the rendered paths rather than
        // hardcoding them.
        let config_dir = Path::new("/tmp/tc-test-xyz");
        let summary = post_install_summary(config_dir, &["~/.zshrc".to_string()], 1);
        for token in [
            "termcmp installed successfully",
            "doctor",
            "Ctrl+/",
            "config.toml",
            "themes",
            "source ~/.zshrc",
            "/tmp/tc-test-xyz/config.toml",
            "/tmp/tc-test-xyz/themes",
        ] {
            assert!(
                summary.contains(token),
                "missing token: {token}\n--- summary ---\n{summary}"
            );
        }
    }

    #[test]
    fn test_post_install_summary_manual_fallback_omits_source_zshrc() {
        let summary = post_install_summary(Path::new("/tmp/cfg"), &[], 1);
        assert!(summary.contains("after pasting the blocks above"));
        assert!(
            !summary.contains("source ~/.zshrc"),
            "manual-fallback summary must not instruct user to source a file \
             they didn't write to:\n{summary}"
        );
        // Headline must reflect the partial-install state — a green-check
        // "installed successfully" headline would contradict the prior
        // permission-denied warning the user just saw on screen.
        assert!(
            summary.contains("partially installed"),
            "manual-fallback headline must signal the degraded state:\n{summary}"
        );
        assert!(
            !summary.contains("installed successfully"),
            "manual-fallback summary must not claim success:\n{summary}"
        );
    }

    #[test]
    fn report_env_respects_per_value_and_total_budgets() {
        assert!(
            ZSH_INTEGRATION.contains("_TC_ENV_TOTAL_BUDGET"),
            "_tc_report_env must declare a total byte budget"
        );
        assert!(
            ZSH_INTEGRATION.contains("_TC_ENV_PER_VALUE_CAP"),
            "_tc_report_env must declare a per-value cap"
        );
    }

    #[test]
    fn test_post_install_summary_uses_sanitized_paths() {
        // Pin the sanitization invariant. We can't blanket-assert
        // `!contains('\x1b')` because the helper intentionally emits ANSI
        // sigils (green check, bolds, dim placeholder); instead pin both
        // directions — the path's raw sequence is gone, the sanitised form
        // is present.
        let hostile = Path::new("/tmp/\x1b[31mevil");
        let summary = post_install_summary(hostile, &["~/.zshrc".to_string()], 1);
        assert!(
            summary.contains("/tmp/[31mevil"),
            "expected sanitised hostile path in summary: {summary:?}"
        );
        assert!(
            !summary.contains("\x1b[31m"),
            "raw ESC sequence from hostile path leaked: {summary:?}"
        );
    }

    #[test]
    fn zle_install_hook_emits_diagnostic_on_non_user_widget() {
        assert!(
            ZSH_INTEGRATION.contains("7774;zle_hook_disabled"),
            "_tc_install_zle_hook must emit OSC 7774 zle_hook_disabled when bailing on a non-user widget"
        );
    }

    #[test]
    fn install_zshrc_write_is_atomic_visible_state() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join(".zshrc");
        std::fs::write(&target, b"original\n").unwrap();
        atomic_write::atomic_write_preserving_mode(&target, b"new\n").unwrap();
        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content, "new\n");
        let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn detect_shell_targets_finds_existing_only() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();

        // Empty home → no targets
        assert_eq!(detect_shell_targets(home), vec![]);

        // Only .bashrc present
        let bashrc = home.join(".bashrc");
        fs::write(&bashrc, "export X=1\n").unwrap();
        assert_eq!(
            detect_shell_targets(home),
            vec![ShellTarget::Bash(bashrc.clone())]
        );

        // All three present → Zsh, Bash, Fish in that order
        let zshrc = home.join(".zshrc");
        fs::write(&zshrc, "export A=1\n").unwrap();
        let fish_dir = home.join(".config").join("fish");
        fs::create_dir_all(&fish_dir).unwrap();
        let fish_config = fish_dir.join("config.fish");
        fs::write(&fish_config, "set -x C 3\n").unwrap();
        assert_eq!(
            detect_shell_targets(home),
            vec![
                ShellTarget::Zsh(zshrc),
                ShellTarget::Bash(bashrc),
                ShellTarget::Fish(fish_config),
            ]
        );
    }

    #[test]
    fn install_writes_every_existing_rc() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();
        let config = home.join(".config/termcmp");

        let zshrc = home.join(".zshrc");
        let bashrc = home.join(".bashrc");
        let fish_dir = home.join(".config").join("fish");
        fs::create_dir_all(&fish_dir).unwrap();
        let fish_config = fish_dir.join("config.fish");

        let zsh_user = "export ZSH_USER=1\n";
        let bash_user = "export BASH_USER=2\n";
        let fish_user = "set -x FISH_USER 3\n";
        fs::write(&zshrc, zsh_user).unwrap();
        fs::write(&bashrc, bash_user).unwrap();
        fs::write(&fish_config, fish_user).unwrap();

        let targets = vec![
            ShellTarget::Zsh(zshrc.clone()),
            ShellTarget::Bash(bashrc.clone()),
            ShellTarget::Fish(fish_config.clone()),
        ];
        install_to(&targets, &config, false).unwrap();

        // zsh: both blocks, user content survives
        let zsh_content = fs::read_to_string(&zshrc).unwrap();
        assert!(zsh_content.contains(INIT_BEGIN));
        assert!(zsh_content.contains(SHELL_BEGIN));
        assert!(zsh_content.contains("export ZSH_USER=1"));

        // bash: integration block only (no init), user content survives
        let bash_content = fs::read_to_string(&bashrc).unwrap();
        assert!(bash_content.contains(SHELL_BEGIN));
        assert!(!bash_content.contains(INIT_BEGIN));
        assert!(bash_content.contains("export BASH_USER=2"));

        // fish: both blocks, user content survives
        let fish_content = fs::read_to_string(&fish_config).unwrap();
        assert!(fish_content.contains(INIT_BEGIN));
        assert!(fish_content.contains(SHELL_BEGIN));
        assert!(fish_content.contains("set -x FISH_USER 3"));

        // Backups hold the originals
        assert_eq!(
            fs::read_to_string(zshrc.with_extension("backup.termcmp")).unwrap(),
            zsh_user
        );
        assert_eq!(
            fs::read_to_string(bashrc.with_extension("backup.termcmp")).unwrap(),
            bash_user
        );
        // with_extension replaces .fish → config.backup.termcmp
        assert_eq!(
            fs::read_to_string(fish_config.with_extension("backup.termcmp")).unwrap(),
            fish_user
        );
    }

    #[test]
    fn install_bash_idempotent() {
        let dir = TempDir::new().unwrap();
        let bashrc = dir.path().join(".bashrc");
        let config = dir.path().join("config");

        fs::write(&bashrc, "export FOO=bar\n").unwrap();

        install_to(&[ShellTarget::Bash(bashrc.clone())], &config, false).unwrap();
        let first = fs::read_to_string(&bashrc).unwrap();

        install_to(&[ShellTarget::Bash(bashrc.clone())], &config, false).unwrap();
        let second = fs::read_to_string(&bashrc).unwrap();

        assert_eq!(first, second, "reinstall must be byte-identical");
        assert_eq!(
            second.matches(SHELL_BEGIN).count(),
            1,
            "exactly one managed block after reinstall"
        );
    }

    #[test]
    fn install_never_creates_missing_rc() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();
        let config = home.join("config");

        // Only .zshrc exists; .bashrc and config.fish must not be created
        let zshrc = home.join(".zshrc");
        fs::write(&zshrc, "export A=1\n").unwrap();

        install_to(&[ShellTarget::Zsh(zshrc.clone())], &config, false).unwrap();

        assert!(!home.join(".bashrc").exists());
        assert!(!home
            .join(".config")
            .join("fish")
            .join("config.fish")
            .exists());
    }
}

#[cfg(test)]
mod drift_tests {
    use super::DEFAULT_CONFIG_TOML;
    use config::all_field_paths;

    #[test]
    fn install_template_contains_every_schema_field() {
        let mut missing = Vec::new();
        for path in all_field_paths() {
            let (_section, key) = path.rsplit_once('.').expect("dotted path");
            // Field is "present" if its bare key appears in the template
            // (commented-out lines count — the template is documentation).
            if !DEFAULT_CONFIG_TOML.contains(key) {
                missing.push(path);
            }
        }
        assert!(
            missing.is_empty(),
            "install template missing keys: {:#?}",
            missing,
        );
    }
}
