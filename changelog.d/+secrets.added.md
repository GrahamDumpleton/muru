Added secrets: stores of the file, env and exec kinds in the daemon's
configuration, secret references in a release's env table or with
`deploy --secret`, delivered as variables or files when the release
starts and never recorded or shown, and `muructl secret` for file stores.
