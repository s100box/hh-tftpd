# hh-tftpd contributor guide

Read [README.md](README.md) and [the design](doc/hh-tftpd-design.md) before
changing protocol behavior or command-line options. Preserve the `tftpd`
executable name and existing CLI for workspace consumers. Run the documented
format, Clippy, Rustdoc and test gates. Tests must use loopback and temporary
files; do not interrupt an existing TFTP server to validate changes.
