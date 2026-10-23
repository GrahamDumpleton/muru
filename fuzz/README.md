# Fuzz targets

cargo-fuzz targets for request heads, header normalisation, forwarding header
parsing, multipart parsing, host patterns, configuration files and protocol
frames (design specification section 15.1). Initialise with
`just bootstrap-fuzz` and `cargo +nightly fuzz init` when the first target is
written, and run one with `just fuzz TARGET`.
