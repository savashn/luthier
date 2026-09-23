//! Core value types shared by the manifest schema and the rest of the system.

use crate::macros::string_enum;
use serde::{Deserialize, Serialize};

string_enum! {
    /// A plugin format an artifact can deliver.
    ///
    /// Adding a format here is deliberately cheap: the installer registry in
    /// `luthier-core` maps a format to a [`FormatInstaller`], and anything without
    /// one is reported as unsupported rather than mis-installed.
    pub enum Format {
        /// CLAP. On Linux a single `*.clap` ELF shared object.
        Clap => "clap",
        /// VST3. On Linux a `*.vst3` bundle directory.
        Vst3 => "vst3",
        /// LV2. A `*.lv2` bundle directory, identified by its `manifest.ttl`.
        Lv2 => "lv2",
        /// Sample library, preset or soundfont content. Not a plugin format
        /// and not scanned as one: it installs under the library root rather
        /// than a plugin root, and no host looks for it by extension.
        Library => "library",
    }
}

string_enum! {
    /// Content that makes no sound on its own and needs an engine to play it.
    ///
    /// The values are the Open Audio Stack registry's `contains` vocabulary
    /// for the same things, so a package read from there needs no
    /// translation. Which engines play each one is listed in
    /// [`builtin_engines`](crate::builtin_engines) and in any bench's
    /// `engines.toml` rather than on this enum: engines come and go far more
    /// often than a binary is released, which is why a bench can add one
    /// without waiting for a release.
    pub enum Content {
        /// An SFZ instrument: `.sfz` text files and the samples they map.
        Sfz => "sfz",
        /// A SoundFont 2 bank.
        Sf2 => "sf2",
        /// A DrumGizmo kit: an XML kit definition over multi-channel WAVs.
        /// There is no file extension to recognise one by, which is why it
        /// is named after the engine that defined the format.
        Drumgizmo => "drumgizmo",
    }
}

impl Content {
  /// How the format is written in prose, for messages.
  pub fn label(&self) -> &str {
    match self {
      Content::Sfz => "SFZ",
      Content::Sf2 => "SoundFont 2",
      Content::Drumgizmo => "DrumGizmo",
      Content::Other(raw) => raw,
    }
  }

  /// What kind of software plays this, named so the sentence is useful on a
  /// machine where no registry knows a single engine.
  ///
  /// A format implies its player: SFZ is played by an SFZ engine, and saying
  /// so needs no registry data at all. What a registry adds is *which* one is
  /// installable right now, which is an improvement on this sentence rather
  /// than a precondition for it.
  pub fn played_by(&self) -> String {
    match self {
      Content::Sfz => "an SFZ engine such as sfizz".into(),
      Content::Sf2 => "a SoundFont player such as FluidSynth".into(),
      Content::Drumgizmo => "DrumGizmo, or another player of its kits".into(),
      Content::Other(raw) => format!("something that reads {raw}"),
    }
  }
}

string_enum! {
    /// Operating system an artifact targets.
    pub enum Os {
        Linux => "linux",
        Macos => "macos",
        Windows => "windows",
    }
}

string_enum! {
    /// CPU architecture an artifact targets.
    pub enum Arch {
        X86_64 => "x86_64",
        Aarch64 => "aarch64",
    }
}

string_enum! {
    /// The one primary classification a package is filed under.
    ///
    /// Deliberately a *closed* vocabulary: validation rejects anything not
    /// listed here, so the registry cannot drift into holding `synth`,
    /// `synthesizer` and `Synth` as three separate groups. Anything finer
    /// grained belongs in `tags`, which stays free-form.
    ///
    /// The `Other` catch-all exists only so a client built against schema v1
    /// can still read a registry that has started using a category added
    /// later (§7); [`crate::validate`] is where an unknown value is refused.
    pub enum Category {
        /// Produces sound: synthesizers, samplers, drum machines.
        Instrument => "instrument",
        /// Processes sound: reverb, EQ, dynamics, distortion.
        Effect => "effect",
        /// Analysers, meters and tools that are neither of the above.
        Utility => "utility",
        /// Sample content: multisampled instruments, drum kits, soundfonts.
        SampleLibrary => "sample-library",
        /// Presets for another package.
        PresetPack => "preset-pack",
        /// A curated set of dependencies and nothing else.
        Pack => "pack",
    }
}

string_enum! {
    /// What a package fundamentally is.
    pub enum PackageKind {
        /// An audio plugin in one or more formats.
        Plugin => "plugin",
        /// A sample library or other data package.
        Library => "library",
        /// A collection of presets for another package.
        PresetPack => "preset-pack",
        /// A standalone application.
        Application => "application",
        /// Metadata only: resolves to a set of dependencies (§49).
        Pack => "pack",
        /// Depended on and detected, never downloaded or installed.
        ///
        /// For software that has no redistributable Linux binary and must come
        /// from a distribution package or a local build (sfizz being the
        /// motivating case). This does not couple us to any distro package
        /// manager (§2.3) — we only detect, never invoke one.
        External => "external",
    }
}

string_enum! {
    /// Container format of a downloaded artifact.
    pub enum ArchiveFormat {
        TarGz => "tar.gz",
        TarXz => "tar.xz",
        Zip => "zip",
        /// LSP Plugins ships Linux binaries in no other container, which is
        /// why this format is supported.
        SevenZ => "7z",
        /// A bare file, not an archive.
        None => "none",
    }
}

string_enum! {
    /// A warning an install rule may deliberately accept.
    ///
    /// The registry's CI runs `--strict`, which makes every warning fatal, and
    /// a warning that is correct in general is occasionally wrong for one real
    /// package. Silencing it per rule keeps `--strict` meaning what it says:
    /// the alternative — dropping `--strict` — would silence every future
    /// warning too, including the ones that catch a genuine mistake.
    ///
    /// Nothing here can relax a rule that guards the user's filesystem. These
    /// name conventions, not safety.
    pub enum AllowedWarning {
        /// The installed name does not carry its format's conventional
        /// extension. LSP Plugins is the case that needs it: its CLAP ships
        /// beside a `.so` that the plugin loads at runtime, so the `.so` must
        /// be installed into the CLAP directory without being a CLAP itself.
        FileExtension => "file-extension",
    }
}

string_enum! {
    /// Whether an install rule moves a single file or a whole directory.
    pub enum EntryKind {
        /// One regular file, e.g. `Surge XT.clap`.
        File => "file",
        /// A directory treated as one unit, e.g. `Surge XT.vst3/`.
        Bundle => "bundle",
    }
}

impl Format {
  /// The layout each format expects on disk.
  pub fn entry_kind(&self) -> Option<EntryKind> {
    match self {
      Format::Clap => Some(EntryKind::File),
      Format::Vst3 | Format::Lv2 => Some(EntryKind::Bundle),
      // Content is whatever upstream ships: a directory of samples, or a
      // single soundfont. Neither shape is wrong, so nothing is enforced.
      Format::Library | Format::Other(_) => None,
    }
  }

  /// The conventional filename extension, without the dot.
  pub fn extension(&self) -> Option<&'static str> {
    match self {
      Format::Clap => Some("clap"),
      Format::Vst3 => Some("vst3"),
      Format::Lv2 => Some("lv2"),
      // No conventional extension, which is also what keeps `scan` from
      // walking library content looking for plugins.
      Format::Library | Format::Other(_) => None,
    }
  }
}

impl ArchiveFormat {
  /// Infer the container from a filename. Order matters: `.tar.gz` must be
  /// tested before `.gz` would be.
  ///
  /// A file that is complete on its own is `none`: a CLAP on Linux is one
  /// shared object, and a SoundFont carries its samples inside it. Nothing
  /// else is. A `.vst3` or `.lv2` is a directory on Linux, so a single file
  /// by that name is an archive mislabelled or a build for another
  /// platform; `.sfz` names samples beside it; `.so` is VST2.
  pub fn from_filename(name: &str) -> Option<Self> {
    let lower = name.to_ascii_lowercase();
    for (suffix, format) in [
      (".tar.gz", ArchiveFormat::TarGz),
      (".tgz", ArchiveFormat::TarGz),
      (".tar.xz", ArchiveFormat::TarXz),
      (".txz", ArchiveFormat::TarXz),
      (".zip", ArchiveFormat::Zip),
      (".7z", ArchiveFormat::SevenZ),
      (".clap", ArchiveFormat::None),
      (".sf2", ArchiveFormat::None),
    ] {
      if lower.ends_with(suffix) {
        return Some(format);
      }
    }
    None
  }
}

/// The platform an artifact is built for.
#[derive(
  Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct Target {
  pub os: Os,
  pub arch: Arch,
}

impl Target {
  pub fn new(os: Os, arch: Arch) -> Self {
    Self { os, arch }
  }

  /// The target this binary is running on, if it is one we model.
  ///
  /// Returns `None` on a platform the schema has no name for, which keeps
  /// the core free of the assumption that it only ever runs on Linux.
  pub fn host() -> Option<Self> {
    let os = match std::env::consts::OS {
      "linux" => Os::Linux,
      "macos" => Os::Macos,
      "windows" => Os::Windows,
      _ => return None,
    };
    let arch = match std::env::consts::ARCH {
      "x86_64" => Arch::X86_64,
      "aarch64" => Arch::Aarch64,
      _ => return None,
    };
    Some(Self { os, arch })
  }
}

impl std::fmt::Display for Target {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}-{}", self.os, self.arch)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn unknown_values_round_trip_verbatim() {
    let f: Format = "sf2".parse().unwrap();
    assert_eq!(f, Format::Other("sf2".into()));
    assert!(!f.is_known());
    assert_eq!(f.as_str(), "sf2");
    // A value we do not understand must survive a parse/serialise cycle so
    // that a newer registry is never silently corrupted by an old client.
    let json = serde_json::to_string(&f).unwrap();
    assert_eq!(json, "\"sf2\"");
    assert_eq!(serde_json::from_str::<Format>(&json).unwrap(), f);
  }

  #[test]
  fn known_values_parse_to_variants() {
    assert_eq!("clap".parse::<Format>().unwrap(), Format::Clap);
    assert_eq!(
      "preset-pack".parse::<PackageKind>().unwrap(),
      PackageKind::PresetPack
    );
    assert_eq!(
      "tar.gz".parse::<ArchiveFormat>().unwrap(),
      ArchiveFormat::TarGz
    );
  }

  #[test]
  fn archive_format_prefers_the_longest_suffix() {
    assert_eq!(
      ArchiveFormat::from_filename("x.tar.gz"),
      Some(ArchiveFormat::TarGz)
    );
    assert_eq!(
      ArchiveFormat::from_filename("x.tar.xz"),
      Some(ArchiveFormat::TarXz)
    );
    assert_eq!(
      ArchiveFormat::from_filename("lsp-plugins-1.2.35-Linux-x86_64.7z"),
      Some(ArchiveFormat::SevenZ)
    );
    assert_eq!(ArchiveFormat::from_filename("plugin.so"), None);
    assert_eq!(
      ArchiveFormat::from_filename("LibreKick_linux_x86_64.clap"),
      Some(ArchiveFormat::None)
    );
    assert_eq!(
      ArchiveFormat::from_filename("Modern.Kit.sf2"),
      Some(ArchiveFormat::None)
    );
    // A bundle on Linux, so never a bare file.
    assert_eq!(ArchiveFormat::from_filename("Plugin.vst3"), None);
  }

  #[test]
  fn formats_declare_their_on_disk_shape() {
    assert_eq!(Format::Clap.entry_kind(), Some(EntryKind::File));
    assert_eq!(Format::Vst3.entry_kind(), Some(EntryKind::Bundle));
    assert_eq!(Format::Other("x".into()).entry_kind(), None);
  }
}
