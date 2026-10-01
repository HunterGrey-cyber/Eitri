# packaging/legacy

`eitri.desktop` is the desktop entry exactly as 0.2.0 shipped it (`packaging/eitri.desktop` at the commit
0.2.0 was cut from), kept byte for byte. Eitri's desktop entry is `cn.huntergrey.eitri.desktop` from the app
icon on, so nothing installs this file under that name any more.

It exists for one reader: **the `install.sh` of 0.2.0**, which refuses a release tarball that has no
`share/applications/eitri.desktop`. A user who saved that installer and reruns it to upgrade would otherwise
be refused by every later release. So `release.sh` puts this file into the release **tarball only**, at that
path; the installer of this release ignores it, and neither nfpm profile nor either AUR package installs it.

What an upgrade through the old installer leaves behind is 0.2.0's entry and no icon; running the new
installer once more removes that entry (it is recognised by being byte for byte what the old installer wrote)
and installs the new entry and the icons. `packaging/release_check.py` holds the tarball's copy to this file.
