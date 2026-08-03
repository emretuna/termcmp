//! Config hot-reload via filesystem watching.
//!
//! Watches `config.toml` for modifications and live-updates the handler's
//! theme, keybindings, trigger chars, popup dimensions, and description-box
//! settings without restarting.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use config::TermcmpConfig;
use notify::{EventKind, RecursiveMode, Watcher};
use overlay::{parse_style, PopupTheme};

use crate::handler::{InputHandler, Keybindings};
use parser::TerminalParser;

/// Handle returned by [`spawn_config_watcher`] that allows the caller to
/// signal the watcher thread to shut down.
pub struct ConfigWatcherHandle {
    shutdown: Arc<AtomicBool>,
    join: tokio::task::JoinHandle<()>,
}

impl ConfigWatcherHandle {
    /// Signal the watcher thread to exit and abort the wrapping task.
    /// The blocking thread checks the flag on each `recv_timeout` cycle
    /// (≤500ms), so it will terminate promptly after this call.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.join.abort();
    }
}

impl Drop for ConfigWatcherHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.join.abort();
    }
}

/// Spawn a background task that watches `config_path` for modifications and
/// hot-reloads runtime-configurable fields into the handler.
///
/// Uses `notify::RecommendedWatcher` with a simple time-based debounce
/// (200ms minimum between reloads) to coalesce rapid write events.
///
/// On reload failure (parse error, invalid theme, etc.) a warning is logged
/// and the previous config is kept. The proxy never crashes from a bad config
/// edit.
pub fn spawn_config_watcher(
    config_path: PathBuf,
    handler: Arc<Mutex<InputHandler>>,
    parser: Arc<Mutex<TerminalParser>>,
) -> Result<ConfigWatcherHandle> {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();

    let watch_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut watcher = notify::RecommendedWatcher::new(tx, notify::Config::default())?;
    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;

    let config_file_name = config_path
        .file_name()
        .map(|f| f.to_os_string())
        .unwrap_or_default();

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let join = tokio::task::spawn_blocking(move || {
        // Keep watcher alive for the lifetime of this blocking task.
        let _watcher = watcher;
        let mut last_reload = Instant::now() - std::time::Duration::from_secs(1);
        let debounce = Duration::from_millis(200);
        let poll_interval = Duration::from_millis(500);

        loop {
            // Check shutdown flag before blocking on recv
            if shutdown_clone.load(Ordering::Acquire) {
                break;
            }

            let event_result = match rx.recv_timeout(poll_interval) {
                Ok(result) => result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let event = match event_result {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("config watcher error: {e}");
                    continue;
                }
            };

            // Only react to modify/create events that touch our config file.
            let dominated_by_config = match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) => event.paths.iter().any(|p| {
                    p.file_name()
                        .map(|f| f == config_file_name)
                        .unwrap_or(false)
                }),
                _ => false,
            };

            if !dominated_by_config {
                continue;
            }

            // Debounce: skip if we reloaded very recently.
            let now = Instant::now();
            if now.duration_since(last_reload) < debounce {
                continue;
            }
            last_reload = now;

            tracing::info!("config.toml changed, reloading...");

            let config = match TermcmpConfig::load(Some(config_path.as_path())) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("config reload failed (parse): {e}");
                    continue;
                }
            };

            match apply_config_reload(&handler, &parser, &config) {
                Ok(()) => {
                    tracing::info!("config reloaded successfully");
                    tracing::debug!(
                        "note: changes to [experimental] require a restart to take effect"
                    );
                }
                Err(e) => {
                    tracing::warn!("config reload failed: {e}");
                }
            }
        }
    });

    Ok(ConfigWatcherHandle { shutdown, join })
}

/// Apply a freshly-loaded [`TermcmpConfig`] to the live handler.
///
/// This is the seam exercised by an actual hot-reload: it resolves the theme
/// and keybindings, forwards every runtime-configurable field through
/// [`InputHandler::update_config`], and writes any cleanup bytes the handler
/// staged. Extracted from the file-watcher loop so it is callable (and
/// testable) without driving `notify` events.
///
/// Returns `Err` on theme/keybinding resolution failure or a poisoned handler
/// lock; the caller keeps the previous config and never crashes.
fn apply_config_reload(
    handler: &Arc<Mutex<InputHandler>>,
    parser: &Arc<Mutex<TerminalParser>>,
    config: &TermcmpConfig,
) -> Result<()> {
    let resolved_theme = config
        .theme
        .resolve(config::config_dir().as_deref())
        .map_err(|e| anyhow::anyhow!("theme preset: {e}"))?;

    let theme = build_popup_theme(
        &resolved_theme,
        config.popup.borders,
        config.popup.border_radius,
        config.popup.spinner,
        config.popup.show_provider_errors,
        config.popup.index_hints,
        config.popup.key_hints,
        config.popup.nerd_icons,
    )
    .map_err(|e| anyhow::anyhow!("theme styles: {e}"))?;

    let keybindings = Keybindings::from_config(&config.keybindings)
        .map_err(|e| anyhow::anyhow!("keybindings: {e}"))?;

    let (overlay_bytes, ticket) = {
        let mut h = handler
            .lock()
            .map_err(|e| anyhow::anyhow!("handler lock poisoned: {e}"))?;
        let mut overlay_bytes = h.update_config(
            theme,
            keybindings,
            config.popup.max_visible,
            config.popup.feedback_dismiss_ms,
            config.trigger.auto_trigger,
            config.popup.min_width,
            config.popup.max_width,
            config.popup.description_box,
            config.popup.description_box_max_width,
            config.popup.description_box_lines,
            config.popup.description_box_debounce_ms,
            config.popup.render_block_ms as u64,
            config.popup.tab_accepts_top,
        );
        // Live-reloadable suggestion settings: result caps, match mode,
        // source-group order, and the commands/filesystem provider toggles
        // all flow through the engine's atomic config swap.
        h.set_live_suggest_config(suggest::LiveSuggestConfig {
            max_results: config.suggest.max_results,
            max_history_results: config.suggest.max_history_results,
            match_mode: config.suggest.match_mode,
            source_order: suggest::SourceOrder::from_names(&config.suggest.order),
            providers_commands: config.suggest.providers.commands,
            providers_filesystem: config.suggest.providers.filesystem,
        });
        h.set_delay_ms(config.trigger.delay_ms);
        // Rebuild the async provider set (LLM + shell-native completions) so
        // [ai] and suggest.providers.shell_completions changes take effect.
        let shell_kind = h.shell_kind();
        let (async_providers, ask_ai, completion_cache) =
            crate::proxy::build_providers(config, shell_kind);
        h.set_async_providers(async_providers);
        h.set_ask_ai_provider(ask_ai);
        h.set_completion_cache(completion_cache.clone());
        // A fresh cache instance gets its own periodic flush task (Weak ref,
        // so the previous cache's task exits once its Arc is dropped here).
        if let Some(cache) = completion_cache {
            crate::shell_completion::spawn_flush_task(&cache);
        }
        // Repaint a visible popup so theme changes (e.g. transparency)
        // take effect immediately instead of waiting for the next keystroke.
        h.repaint_visible_into(parser, &mut overlay_bytes);
        (overlay_bytes, h.overlay_write_ticket())
    };
    if !overlay_bytes.is_empty() {
        if let Err(e) = crate::proxy::write_overlay_if_current(handler, ticket, &overlay_bytes) {
            tracing::debug!("config reload overlay write/flush failed: {e}");
        }
    }

    Ok(())
}

/// Build a `PopupTheme` from a [`config::ResolvedTheme`] (preset merged
/// with user overrides), parsing each style string.
#[allow(clippy::too_many_arguments)]
fn build_popup_theme(
    resolved: &config::ResolvedTheme,
    borders: bool,
    border_radius: bool,
    spinner: bool,
    show_provider_errors: bool,
    index_hints: bool,
    key_hints: bool,
    nerd_icons: bool,
) -> Result<PopupTheme> {
    Ok(PopupTheme {
        selected_on: parse_style(&resolved.selected)
            .map_err(|e| anyhow::anyhow!("invalid theme.selected: {e}"))?,
        description_on: parse_style(&resolved.description)
            .map_err(|e| anyhow::anyhow!("invalid theme.description: {e}"))?,
        feedback_loading_on: parse_style(&resolved.feedback_loading)
            .map_err(|e| anyhow::anyhow!("invalid theme.feedback_loading: {e}"))?,
        feedback_empty_on: parse_style(&resolved.feedback_empty)
            .map_err(|e| anyhow::anyhow!("invalid theme.feedback_empty: {e}"))?,
        feedback_error_on: parse_style(&resolved.feedback_error)
            .map_err(|e| anyhow::anyhow!("invalid theme.feedback_error: {e}"))?,
        match_highlight_on: parse_style(&resolved.match_highlight)
            .map_err(|e| anyhow::anyhow!("invalid theme.match_highlight: {e}"))?,
        item_text_on: parse_style(&resolved.item_text)
            .map_err(|e| anyhow::anyhow!("invalid theme.item_text: {e}"))?,
        scrollbar_on: parse_style(&resolved.scrollbar)
            .map_err(|e| anyhow::anyhow!("invalid theme.scrollbar: {e}"))?,
        border_on: parse_style(&resolved.border)
            .map_err(|e| anyhow::anyhow!("invalid theme.border: {e}"))?,
        borders,
        border_radius,
        spinner,
        show_provider_errors,
        background_on: parse_style(&resolved.background)
            .map_err(|e| anyhow::anyhow!("invalid theme.background: {e}"))?,
        description_box_background_on: parse_style(&resolved.description_box_background)
            .map_err(|e| anyhow::anyhow!("invalid theme.description_box_background: {e}"))?,
        kind_icon_on: parse_style(&resolved.kind_icon)
            .map_err(|e| anyhow::anyhow!("invalid theme.kind_icon: {e}"))?,
        index_hints,
        key_hints,
        nerd_icons,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::ShellKind;
    use suggest::{Suggestion, SuggestionKind, SuggestionSource};

    #[test]
    fn test_build_popup_theme_valid() {
        let resolved = config::ResolvedTheme {
            selected: "reverse".into(),
            description: "dim".into(),
            match_highlight: "bold".into(),
            item_text: String::new(),
            scrollbar: "dim".into(),
            border: "dim".into(),
            feedback_loading: "dim".into(),
            feedback_empty: "dim".into(),
            feedback_error: "dim fg:#f38ba8".into(),
            kind_icon: "bold".into(),
            background: String::new(),
            description_box_background: String::new(),
        };
        let result = build_popup_theme(&resolved, true, true, true, false, true, true, true);
        assert!(result.is_ok());
        let theme = result.unwrap();
        assert_eq!(theme.kind_icon_on, b"\x1b[1m");
        assert!(theme.borders);
    }

    /// A live config reload must carry `popup.render_block_ms` all the way
    /// through `apply_config_reload` into the handler. Exercises the real
    /// `TermcmpConfig -> config_watch -> InputHandler` seam — not a direct
    /// `update_config` call — so the test fails if `apply_config_reload`
    /// ever stops forwarding the field.
    #[test]
    fn render_block_ms_propagates_on_config_reload() {
        let handler = Arc::new(Mutex::new(
            InputHandler::new(terminal::TerminalProfile::for_ghostty(), ShellKind::Other)
                .expect("handler builds"),
        ));
        let parser = Arc::new(Mutex::new(TerminalParser::new(24, 80)));
        assert_eq!(
            handler.lock().unwrap().render_block_ms(),
            80,
            "default render_block_ms should be 80"
        );

        let mut config = TermcmpConfig::default();
        config.popup.render_block_ms = 150;

        apply_config_reload(&handler, &parser, &config).expect("config reload applies");

        assert_eq!(handler.lock().unwrap().render_block_ms(), 150);
    }

    /// `popup.tab_accepts_top` must travel the same `TermcmpConfig ->
    /// config_watch -> InputHandler` seam as `render_block_ms` — it is the
    /// last positional arg threaded through `update_config`, so this guards
    /// against `apply_config_reload` dropping it on a live reload.
    #[test]
    fn tab_accepts_top_propagates_on_config_reload() {
        let handler = Arc::new(Mutex::new(
            InputHandler::new(terminal::TerminalProfile::for_ghostty(), ShellKind::Other)
                .expect("handler builds"),
        ));
        let parser = Arc::new(Mutex::new(TerminalParser::new(24, 80)));
        assert!(
            !handler.lock().unwrap().tab_accepts_top(),
            "default tab_accepts_top should be false"
        );

        let mut config = TermcmpConfig::default();
        config.popup.tab_accepts_top = true;

        apply_config_reload(&handler, &parser, &config).expect("config reload applies");

        assert!(
            handler.lock().unwrap().tab_accepts_top(),
            "reload should propagate tab_accepts_top = true into the handler"
        );
    }

    /// Drives the full hot-reload seam end-to-end: an on-disk config.toml,
    /// the real `notify::RecommendedWatcher` (kind filter + 200ms debounce +
    /// `TermcmpConfig::load`), and the handler update. The pure-Rust test
    /// above (`render_block_ms_propagates_on_config_reload`) only exercises
    /// `apply_config_reload`; if the event-kind filter or `file_name` match
    /// silently drops a `Modify` event for an in-place rewrite, the
    /// single-field reload never reaches `update_config` in production —
    /// this test catches that regression.
    ///
    /// Timeout is 5s to tolerate slow CI; the notify backend on macOS is
    /// fsevents and typically delivers events within ~100ms.
    #[test]
    fn render_block_ms_propagates_through_file_watcher() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let config_path = dir.path().join("config.toml");

            // Seed with a starting value so the second write is an in-place
            // Modify (not a Create) — exercises the EventKind::Modify branch
            // of the kind filter.
            std::fs::write(&config_path, "[popup]\nrender_block_ms = 90\n").expect("seed config");

            let handler = Arc::new(Mutex::new(
                InputHandler::new(terminal::TerminalProfile::for_ghostty(), ShellKind::Other)
                    .expect("handler builds"),
            ));
            let parser = Arc::new(Mutex::new(TerminalParser::new(24, 80)));
            // Sanity: default render_block_ms before the watcher fires.
            assert_eq!(handler.lock().unwrap().render_block_ms(), 80);

            let watcher_handle = spawn_config_watcher(
                config_path.clone(),
                Arc::clone(&handler),
                Arc::clone(&parser),
            )
            .expect("watcher spawns");

            // The 200ms debounce treats events as "recent" if last_reload
            // was within the window. Wait past it before the meaningful
            // write to guarantee this edit is not coalesced with a startup
            // event (notify sometimes emits a creation/touch event during
            // watcher registration).
            tokio::time::sleep(Duration::from_millis(300)).await;

            // Trigger the reload: rewrite with the target value.
            std::fs::write(&config_path, "[popup]\nrender_block_ms = 150\n")
                .expect("rewrite config");

            // Poll the handler for up to 5s for the new value to land.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut observed = handler.lock().unwrap().render_block_ms();
            while observed != 150 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
                observed = handler.lock().unwrap().render_block_ms();
            }

            watcher_handle.shutdown();

            assert_eq!(
                observed, 150,
                "watcher did not propagate render_block_ms within 5s; \
                 the notify event-kind filter or file_name match likely \
                 dropped the Modify event"
            );
        });
    }

    /// Editors that save via write-temp-then-rename (vim, nano, VS Code,
    /// `termcmp config edit`'s own `atomic_write`) replace `config.toml`'s
    /// inode, which `notify` reports as `EventKind::Modify(ModifyKind::Name(
    /// RenamedTo))` — or a `Create` — NOT a plain data `Modify`. The watcher's
    /// kind filter must accept those, otherwise the most common save path
    /// silently never hot-reloads. Mirrors `render_block_ms_propagates_through
    /// _file_watcher` but performs the edit as an atomic rename.
    #[test]
    fn atomic_rename_save_propagates_through_file_watcher() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let config_path = dir.path().join("config.toml");

            std::fs::write(&config_path, "[popup]\nrender_block_ms = 90\n").expect("seed config");

            let handler = Arc::new(Mutex::new(
                InputHandler::new(terminal::TerminalProfile::for_ghostty(), ShellKind::Other)
                    .expect("handler builds"),
            ));
            let parser = Arc::new(Mutex::new(TerminalParser::new(24, 80)));
            assert_eq!(handler.lock().unwrap().render_block_ms(), 80);

            let watcher_handle = spawn_config_watcher(
                config_path.clone(),
                Arc::clone(&handler),
                Arc::clone(&parser),
            )
            .expect("watcher spawns");

            tokio::time::sleep(Duration::from_millis(300)).await;

            // Atomic rename save: write a sibling temp file, then rename over
            // config.toml — the exact pattern editors and atomic_write use.
            let tmp_path = dir.path().join("config.toml.tmp");
            std::fs::write(&tmp_path, "[popup]\nrender_block_ms = 150\n").expect("write temp");
            std::fs::rename(&tmp_path, &config_path).expect("atomic rename");

            let deadline = Instant::now() + Duration::from_secs(5);
            let mut observed = handler.lock().unwrap().render_block_ms();
            while observed != 150 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
                observed = handler.lock().unwrap().render_block_ms();
            }

            watcher_handle.shutdown();

            assert_eq!(
                observed, 150,
                "watcher did not propagate an atomic-rename save within 5s; \
                 the event-kind filter likely ignores RenamedTo/Create events \
                 that editors emit when saving via write-temp-then-rename"
            );
        });
    }

    /// Toggling `theme.transparency` on a live, visible popup must repaint
    /// immediately — not wait for the next keystroke. The reload resolves the
    /// theme (clearing backgrounds) and swaps it via `update_config`, but the
    /// popup on screen only changes if `apply_config_reload` repaints it. This
    /// guards that repaint: the output epoch must bump (a render happened) and
    /// the handler's theme must carry the cleared background.
    #[test]
    fn transparency_change_repaints_visible_popup() {
        let handler = Arc::new(Mutex::new(
            InputHandler::new(terminal::TerminalProfile::for_ghostty(), ShellKind::Other)
                .expect("handler builds"),
        ));
        let parser = Arc::new(Mutex::new(TerminalParser::new(24, 80)));

        // Make the popup visible so the reload has something to repaint.
        handler
            .lock()
            .unwrap()
            .force_visible_for_test(vec![Suggestion {
                text: "checkout".to_string(),
                kind: SuggestionKind::Command,
                source: SuggestionSource::Commands,
                ..Default::default()
            }]);
        assert!(handler.lock().unwrap().is_visible());

        let epoch_before = handler.lock().unwrap().output_epoch();

        // A themed background, then transparency on — resolve() clears it.
        let mut config = TermcmpConfig::default();
        config.theme.name = "catppuccin".into();
        config.theme.transparency = true;

        apply_config_reload(&handler, &parser, &config).expect("config reload applies");

        let h = handler.lock().unwrap();
        assert!(
            h.theme().background_on.is_empty(),
            "transparency must clear the popup background on reload"
        );
        assert_ne!(
            h.output_epoch(),
            epoch_before,
            "a visible popup must be repainted when the theme changes on reload"
        );
    }
}
