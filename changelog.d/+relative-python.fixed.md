Fixed a relative `--python` for `muructl deploy`, or a relative `python`
in a file for `muructl apply`, being resolved where the daemon starts
the host rather than where the command ran or the file is; a bare
command name is still looked up on the PATH.
