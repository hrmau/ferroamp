//! Plain text settings and the session playlist, in Application Support.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::engine::eq::BANDS;
use crate::engine::gain::ReplayGain;
use crate::playlist::Repeat;

/// What the small display in the main window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainVis {
    Spectrum,
    Scope,
    Off,
}

impl MainVis {
    pub fn next(self) -> Self {
        match self {
            MainVis::Spectrum => MainVis::Scope,
            MainVis::Scope => MainVis::Off,
            MainVis::Off => MainVis::Spectrum,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub volume: f32,
    pub balance: f32,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Seconds of overlap between tracks, or 0 for none.
    pub crossfade: u32,
    pub replay_gain: ReplayGain,
    pub always_on_top: bool,
    pub shade_main: bool,
    pub shade_eq: bool,
    pub shade_playlist: bool,
    /// A `.wsz` file or folder to draw the windows with, instead of the built-in look.
    pub skin: Option<PathBuf>,
    pub eq_on: bool,
    pub preamp: f32,
    pub gains: [f32; BANDS],
    pub preset: usize,
    pub main_vis: MainVis,
    pub show_visuals: bool,
    pub visual_preset: usize,
    pub visuals_pos: Option<(i32, i32)>,
    pub show_eq: bool,
    pub show_playlist: bool,
    pub time_remaining: bool,
    pub zoom: u32,
    pub current_index: Option<usize>,
    pub main_pos: Option<(i32, i32)>,
    pub eq_pos: Option<(i32, i32)>,
    pub playlist_pos: Option<(i32, i32)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.8,
            balance: 0.0,
            shuffle: false,
            repeat: Repeat::Off,
            crossfade: 0,
            replay_gain: ReplayGain::Track,
            always_on_top: false,
            shade_main: false,
            shade_eq: false,
            shade_playlist: false,
            skin: None,
            eq_on: false,
            preamp: 0.0,
            gains: [0.0; BANDS],
            preset: 0,
            main_vis: MainVis::Spectrum,
            show_visuals: false,
            visual_preset: 0,
            visuals_pos: None,
            show_eq: true,
            show_playlist: true,
            time_remaining: false,
            zoom: 2,
            current_index: None,
            main_pos: None,
            eq_pos: None,
            playlist_pos: None,
        }
    }
}

fn pos(map: &HashMap<&str, &str>, key: &str) -> Option<(i32, i32)> {
    let (x, y) = map.get(key)?.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

impl Settings {
    pub fn parse(text: &str) -> Self {
        let map: HashMap<&str, &str> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let d = Self::default();
        let f = |k: &str, v: f32| map.get(k).and_then(|s| s.parse().ok()).unwrap_or(v);
        let b = |k: &str, v: bool| map.get(k).map_or(v, |s| *s == "1");
        let mut gains = d.gains;
        if let Some(list) = map.get("gains") {
            for (slot, v) in gains.iter_mut().zip(list.split(',')) {
                *slot = v.parse().unwrap_or(0.0);
            }
        }
        Self {
            volume: f("volume", d.volume).clamp(0.0, 1.0),
            balance: f("balance", d.balance).clamp(-1.0, 1.0),
            shuffle: b("shuffle", d.shuffle),
            repeat: match map.get("repeat").copied() {
                Some("1") => Repeat::All,
                Some("2") => Repeat::One,
                _ => Repeat::Off,
            },
            crossfade: map.get("crossfade").and_then(|s| s.parse().ok()).unwrap_or(0).min(30),
            replay_gain: match map.get("replay_gain").copied() {
                Some("off") => ReplayGain::Off,
                Some("album") => ReplayGain::Album,
                Some(_) => ReplayGain::Track,
                None => d.replay_gain,
            },
            always_on_top: b("always_on_top", d.always_on_top),
            shade_main: b("shade_main", d.shade_main),
            shade_eq: b("shade_eq", d.shade_eq),
            shade_playlist: b("shade_playlist", d.shade_playlist),
            skin: map.get("skin").filter(|s| !s.is_empty()).map(PathBuf::from),
            eq_on: b("eq_on", d.eq_on),
            preamp: f("preamp", d.preamp),
            gains,
            preset: map.get("preset").and_then(|s| s.parse().ok()).unwrap_or(0),
            main_vis: match map.get("main_vis").copied() {
                Some("scope") => MainVis::Scope,
                Some("off") => MainVis::Off,
                Some(_) => MainVis::Spectrum,
                // Settings written before there was a choice of display.
                None if map.get("vis_on") == Some(&"0") => MainVis::Off,
                None => d.main_vis,
            },
            show_visuals: b("show_visuals", d.show_visuals),
            visual_preset: map.get("visual_preset").and_then(|s| s.parse().ok()).unwrap_or(0),
            visuals_pos: pos(&map, "visuals_pos"),
            show_eq: b("show_eq", d.show_eq),
            show_playlist: b("show_playlist", d.show_playlist),
            time_remaining: b("time_remaining", d.time_remaining),
            zoom: map
                .get("zoom")
                .and_then(|s| s.parse().ok())
                .unwrap_or(d.zoom)
                .clamp(1, 4),
            current_index: map.get("current_index").and_then(|s| s.parse().ok()),
            main_pos: pos(&map, "main_pos"),
            eq_pos: pos(&map, "eq_pos"),
            playlist_pos: pos(&map, "playlist_pos"),
        }
    }

    pub fn serialize(&self) -> String {
        let flag = |v: bool| if v { 1 } else { 0 };
        let gains: Vec<String> = self.gains.iter().map(|g| format!("{g:.2}")).collect();
        let mut s = format!(
            "volume={:.3}\nbalance={:.3}\nshuffle={}\nrepeat={}\ncrossfade={}\nreplay_gain={}\nalways_on_top={}\nshade_main={}\nshade_eq={}\nshade_playlist={}\neq_on={}\npreamp={:.2}\ngains={}\npreset={}\nmain_vis={}\nshow_visuals={}\nvisual_preset={}\nshow_eq={}\nshow_playlist={}\ntime_remaining={}\nzoom={}\n",
            self.volume,
            self.balance,
            flag(self.shuffle),
            match self.repeat {
                Repeat::Off => 0,
                Repeat::All => 1,
                Repeat::One => 2,
            },
            self.crossfade,
            match self.replay_gain {
                ReplayGain::Off => "off",
                ReplayGain::Track => "track",
                ReplayGain::Album => "album",
            },
            flag(self.always_on_top),
            flag(self.shade_main),
            flag(self.shade_eq),
            flag(self.shade_playlist),
            flag(self.eq_on),
            self.preamp,
            gains.join(","),
            self.preset,
            match self.main_vis {
                MainVis::Spectrum => "spectrum",
                MainVis::Scope => "scope",
                MainVis::Off => "off",
            },
            flag(self.show_visuals),
            self.visual_preset,
            flag(self.show_eq),
            flag(self.show_playlist),
            flag(self.time_remaining),
            self.zoom,
        );
        if let Some(path) = &self.skin {
            s.push_str(&format!("skin={}\n", path.display()));
        }
        if let Some(i) = self.current_index {
            s.push_str(&format!("current_index={i}\n"));
        }
        for (key, value) in [
            ("main_pos", self.main_pos),
            ("eq_pos", self.eq_pos),
            ("playlist_pos", self.playlist_pos),
            ("visuals_pos", self.visuals_pos),
        ] {
            if let Some((x, y)) = value {
                s.push_str(&format!("{key}={x},{y}\n"));
            }
        }
        s
    }
}

pub fn config_dir() -> Option<PathBuf> {
    // Tests and side-by-side runs can point the app at a scratch folder.
    if let Some(dir) = std::env::var_os("FERROAMP_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/ferroamp"))
}

/// Brings settings over from the name this app had before.
fn adopt_old_settings(dir: &std::path::Path) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let old = PathBuf::from(home).join("Library/Application Support/macamp");
    if dir.exists() || !old.is_dir() || std::env::var_os("FERROAMP_CONFIG_DIR").is_some() {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    for name in ["settings.conf", "session.m3u8"] {
        let _ = std::fs::copy(old.join(name), dir.join(name));
    }
}

pub fn load() -> (Settings, Vec<crate::playlist::Item>) {
    let Some(dir) = config_dir() else {
        return (Settings::default(), Vec::new());
    };
    adopt_old_settings(&dir);
    let settings = std::fs::read_to_string(dir.join("settings.conf"))
        .map(|t| Settings::parse(&t))
        .unwrap_or_default();
    let session = std::fs::read_to_string(dir.join("session.m3u8"))
        .map(|t| crate::playlist::parse_playlist(&t, &dir))
        .unwrap_or_default();
    (settings, session)
}

pub fn save(settings: &Settings, session_m3u: &str) {
    let Some(dir) = config_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let write = |name: &str, text: &str| {
        // Write then rename, so a crash cannot leave a half-written file.
        let tmp = dir.join(format!("{name}.tmp"));
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(name));
        }
    };
    write("settings.conf", &settings.serialize());
    write("session.m3u8", session_m3u);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut s = Settings {
            volume: 0.25,
            shuffle: true,
            repeat: Repeat::One,
            crossfade: 6,
            replay_gain: ReplayGain::Album,
            always_on_top: true,
            shade_eq: true,
            skin: Some(PathBuf::from("/skins/my skin.wsz")),
            eq_on: true,
            preset: 3,
            current_index: Some(7),
            main_pos: Some((-40, 100)),
            main_vis: MainVis::Scope,
            show_visuals: true,
            visual_preset: 4,
            ..Settings::default()
        };
        s.gains[9] = -3.5;
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn old_settings_keep_their_display_choice() {
        assert_eq!(Settings::parse("vis_on=0\n").main_vis, MainVis::Off);
        assert_eq!(Settings::parse("vis_on=1\n").main_vis, MainVis::Spectrum);
    }

    #[test]
    fn garbage_falls_back_to_defaults() {
        let s = Settings::parse("volume=lots\nzoom=99\nnonsense\nmain_pos=1\n");
        assert_eq!(s.volume, 0.8);
        assert_eq!(s.zoom, 4);
        assert_eq!(s.main_pos, None);
    }
}
