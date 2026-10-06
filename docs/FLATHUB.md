# DAWs installed from Flathub

A DAW from Flathub runs in a sandbox, and still finds what Luthier installs in
`~/.clap`, `~/.vst3` and `~/.lv2`: Ardour, REAPER, LMMS and Qtractor can read
your home directory, and Bitwig and Zrythm the whole system. Checked with
Ardour 9.8 from Flathub, whose own LV2 discovery and VST3 scanner found and
loaded every plugin Luthier had installed there, inside the sandbox.

Three things behave differently from a DAW installed by your distribution:

- **A plugin location outside your home directory is not visible** to a DAW
  that can only read your home, and the search-path variables do not all get
  through either — Ardour and Bitwig set `VST3_PATH`, and Bitwig `CLAP_PATH`,
  themselves. If you moved plugins with `luthier location set plugins`, grant
  the DAW the directory and the path, for example:

  ```console
  $ flatpak override --user --filesystem=/mnt/audio \
      --env=VST3_PATH=/mnt/audio/plugins/vst3:/app/extensions/Plugins/vst3 \
      org.ardour.Ardour
  ```

  The same goes for sample libraries on another disk: the player inside the
  DAW has to be able to read them.
- **A plugin that links a library from your system does not load**, because
  the sandbox has its own libraries. Most plugins bring what they need;
  `fluidsynth-clap` does not, and needs `libfluidsynth` from the system, so
  it works only in a DAW your distribution installed.
- **Plugins from nixpkgs or your distribution are invisible** to a sandboxed
  DAW. Flathub offers many of them as `org.freedesktop.LinuxAudio.Plugins.*`
  extensions instead.
