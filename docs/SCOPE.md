# What it will not do, and why some plugins are missing

Installing a package here means: download it, check it against the checksum in
its manifest, extract it, and copy files into place. That is the complete list.
There is no field in a manifest that runs a command, no post-install hook, no
build step — and none will be added.

The reason is that a registry is a pull request away from every user's machine.
If a manifest could run a command, then merging one would mean running a
stranger's code on every computer that installs it. Nothing about reviewing a
pull request carefully makes that safe enough. So the manager cannot execute
anything, and there is nothing to review for: the worst a merged manifest can
do is put a file in a plugin directory. See [SECURITY.md](../SECURITY.md).

Two consequences follow, and they explain most of what is not here.

**Software that installs itself is out of reach.** A `.deb`, `.rpm` or `.exe`
is a program that unpacks itself and runs scripts as it goes. Running one is
exactly what this manager will not do — and running it as root, which those
formats expect, doubly so.

**Software distributed only by distributions is out of reach.** Guitarix, Calf,
x42-plugins and many other excellent projects publish source, and their binaries
are built by Debian, Arch, Fedora and the rest. Those binaries are real, but
they are built against one distribution's library versions and belong in
`/usr/lib`. Copying Debian's build into `~/.lv2` on Arch produces a file that
installs cleanly and then fails to load, which is the one outcome this manager
is designed never to produce. Your distribution's package manager does this job
properly; there is nothing to gain from doing it badly here.

What is left is what upstream publishes as a portable archive — a build that
carries what it needs and runs anywhere. Surge XT, Dexed, LSP Plugins and
several hundred more do exactly that, and those are the packages you will find.

So: if a plugin is missing, `apt install` or `pacman -S` is usually the answer,
and that is not a workaround. It is the other half of a division of labour.
