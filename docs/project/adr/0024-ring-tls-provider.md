---
status: accepted
date: 2026-10-24
---

# ADR 0024: ring as the cryptographic provider for TLS

## Context and problem statement

rustls implements the TLS protocol but none of the cryptography; it is
built with a provider that supplies the ciphers, hashes, signatures and key
exchange. The rustls project maintains two: ring, a trimmed copy of
BoringSSL's cryptographic code with a Rust layer, and aws-lc-rs, which wraps
Amazon's AWS-LC fork of BoringSSL and has been the rustls default since
0.23. Section 14.2 left the choice open ("ring or aws-lc-rs"). HTTPS in
development (section 13.1) is the first TLS code in Muru, so the choice is
made now, and it also applies to rcgen, which makes the development
certificates, and later to rustls-acme.

## Decision drivers

- Building Muru from source must need only the prerequisites of section
  15.6: a C toolchain, rustup, uv and just. aws-lc-rs builds AWS-LC with
  cmake, and on some platforms needs Go or NASM as well; ring builds with
  the C compiler alone.

- One provider in the whole binary. Two providers duplicate the
  cryptography and force every rustls configuration to name its provider
  explicitly, because rustls refuses to guess between them.

- Licences within the allow list of deny.toml. Both qualify: ring under
  Apache-2.0 and ISC with BSD-style terms for the BoringSSL parts, aws-lc-rs
  under ISC or Apache-2.0.

- FIPS validation, which only aws-lc-rs offers, is not a requirement of
  Muru's intended users: a Python application server on a laptop or a
  server, usually behind nothing or behind a load balancer that terminates
  TLS itself.

## Considered options

- ring, with rustls, tokio-rustls and rcgen built with their `ring` feature
  and their default features off.

- aws-lc-rs, the rustls default, with cmake added to the prerequisites.

- Both, letting each crate keep its default and selecting the provider at
  run time.

## Decision outcome

Chosen option: "ring", because it adds no build prerequisite for
contributors, continuous integration or anyone building from source, and
nothing Muru needs is missing from it.

How it is held in place:

- rustls, tokio-rustls and rcgen are declared once in the workspace
  manifest with default features off and `ring` on, so no crate can quietly
  bring the other provider back through default features.

- Every rustls configuration is built with `builder_with_provider` and the
  provider from `muru_edge::tls::crypto_provider`, never with the
  process-wide default.

- cargo-deny bans aws-lc-rs and aws-lc-sys, so a new dependency that pulls
  in the second provider fails `cargo deny check bans` instead of adding
  cmake to the build.

### Consequences

- Good: the prerequisites of section 15.6 stay as they are, builds are
  faster, and one provider's code is in the binary.

- Bad: no FIPS mode. A user who needs it would terminate TLS in front of
  Muru, or this record would be revisited; the feature flags are confined
  to the workspace manifest, so switching is a small change.

- Bad: the post-quantum key exchange rustls offers (X25519MLKEM768) is
  implemented for aws-lc-rs only at the time of writing, so Muru does not
  offer it until ring does.

## More information

Sections 13.1 (HTTPS in development), 14.2 (key dependencies) and 15.6
(prerequisites). The ban is checked by `cargo deny check bans`, which
`just lint` and continuous integration run.
