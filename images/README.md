# Images

- `dev/`: the development image used by the devcontainer and `just test-linux`
  (design specification section 15.6).
- `test/`: the image the container scenarios of the integration suite build
  (section 15.2).
- There are no base images for applications and no `muructl` image: an
  application's Dockerfile installs `muructl` with `uv tool install`
  (section 8.6, ADR 0046).
