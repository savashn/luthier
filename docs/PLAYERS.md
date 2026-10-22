# Opening libraries in a player

A sample library is not a plugin. No host scans for one, and installing it
tells no player it is there. Luthier installs each library as a folder in
the library location, and you point a player at a file inside it. A
library from the Open Audio Stack registry gets a folder named after its
package ID; one from extras keeps the name of the folder in its archive,
so `muldjordkit` goes to `MuldjordKit3`. Ask Luthier rather than guess
([Finding the folder](#finding-the-folder)). The player stores that file in your project, so this is once per
instrument, not once per session. To skip even that, save the track with
its player as a template in your DAW.

Checked against sfizz's current source and its 1.2.3 build, Carla 2.5.10,
and the DrumGizmo 0.9.20 binary.

- [Finding the folder](#finding-the-folder)
- [sfizz](#sfizz)
- [DrumGizmo](#drumgizmo)
- [SoundFonts](#soundfonts)
- [Single samples](#single-samples)
- [Without any setting](#without-any-setting)

## Finding the folder

The library location is `~/.local/share/luthier/libraries` (under
`$XDG_DATA_HOME`), unless `luthier location set libraries` moved it.
`luthier install` prints a library's folder under the line saying it was
installed, and you can ask for it again at any time:

```console
$ luthier list vsco-2-ce        # where one package went
/home/you/.local/share/luthier/libraries/vsco-2-ce
$ luthier location              # every location, libraries included
```

`luthier info <package>` shows the same path as `Location`, and under
`Played by` the engines that play what the library holds. Any one of them
is enough.

The directory is hidden, so a file chooser does not show it unless asked
to. Type or paste the path into the chooser's location field instead of
clicking down to it.

## sfizz

For SFZ libraries. sfizz has no library browser; it opens one `.sfz` file.
Its file chooser starts in the folder of the instrument already loaded,
otherwise in its *User SFZ folder*, otherwise in
`$XDG_DOCUMENTS_DIR/SFZ instruments` (usually `~/Documents/SFZ
instruments`). So set the folder once:

1. Open sfizz's **Settings** panel.
2. Click the button beside **User SFZ folder** and choose the library
   location.

From then on **Load file** opens there, and **Open SFZ folder** in the same
menu opens it in your file manager. The arrows beside the file name step
through the other `.sfz` files in the folder of the one loaded, which is
how to try the instruments of a library such as VSCO 2 that ships one file
per instrument.

`SFZ_PATH`, a colon-separated list, is searched with those two folders,
subfolders included, but only to find an instrument that moved when a
session is opened again. It lists nothing.

## DrumGizmo

For DrumGizmo kits. DrumGizmo loads a kit from two XML files, **Drumkit
file** and **Midimap file**, each chosen with **Browse...**. Both are in the
kit's folder under the library location.

## SoundFonts

sfizz does not play SoundFonts. Carla does, as a plugin inside the DAW, and
it is the one player here that finds libraries itself: every `.sf2` and
`.sf3` under its SF2 paths, subfolders included, appears in its plugin list
as a sound kit.

Add the library location to the SF2 list under **Paths** in Carla's
settings. Or set `SF2_PATH` before Carla starts. It replaces Carla's own
defaults, so list again the ones you still want:

```sh
export SF2_PATH="$HOME/.local/share/luthier/libraries:$HOME/.sounds/sf2:/usr/share/soundfonts"
```

Carla's defaults are `~/.sounds/sf2`, `~/.sounds/sf3`, `/usr/share/sounds/sf2`,
`/usr/share/sounds/sf3` and `/usr/share/soundfonts`. A list saved in Carla's
settings wins over the variable. A variable set in a shell profile reaches
a DAW started from the desktop only after you log out and back in.

## Single samples

A one-shot needs no player: drag the file onto a track. Add the library
location to your DAW's file browser as a shortcut, for example to REAPER's
Media Explorer.

## Without any setting

sfizz looks in `~/Documents/SFZ instruments` with nothing set, so Luthier
can install there instead:

```console
$ mkdir -p ~/Documents/"SFZ instruments"
$ luthier location set libraries ~/Documents/"SFZ instruments"
```

The library folder is then also visible, outside the hidden directory. It
is for every library, though, not only SFZ: DrumGizmo kits and SoundFonts
go there too. Luthier never creates a location you chose, hence the
`mkdir`, and moving a location that packages are installed in is refused,
so do this before installing any library.

A DAW installed from Flathub reads your home directory, so it finds either
location. One outside your home needs the override in
[DAWs installed from Flathub](FLATHUB.md).
