# Frequently asked questions

**Does it make every download faster?** Only when there is more than one
route to the internet (see [using several connections](multi-link.md)),
or when a server limits the speed of each single connection; then
several connections to it help even on one network.

**Can I close the app during a download?** Yes. Progress is saved as it
goes. Downloads continue when you start the app again (Settings → resume
unfinished downloads when the app starts).

**The disk filled up. Did I lose the download?** No. The package pauses
and says why. Free some space and press resume.

**Where are my settings and queue?** In the data folder listed in the
README. Removing that folder resets the app.

**Can I use the command line and the app together?** They share the same
queue, so only one of them can run downloads at a time. The other says
which one is busy.

**Why is RAR not extracted?** RAR extraction needs the UnRAR library,
whose licence is not open source, so release builds leave it out. ZIP
and 7z are extracted. Source builds can turn RAR on (see the README).

**Does it download from file-hosting sites, video sites or streams?** It
downloads files from links you already have access to. It does not and
will not bypass captchas or waiting pages, break DRM or stream
protection, or share or crack premium accounts.

**Does it send anything anywhere?** No telemetry. The only connections
are the downloads themselves and, in copies that update themselves, a
check of the project's GitHub Releases page at startup (Settings →
About can turn it off).

**How do I report a bug?** Open an issue on GitHub. Settings →
Diagnostics gives a report to paste in; links in it are cut down to host
and path.
