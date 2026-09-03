# Security Policy

## Reporting a Vulnerability

consolette proxies requests to LLM upstreams and handles API credentials (bearer tokens, AWS credentials, exec-based credential helpers). If you find a security vulnerability, please **do not open a public issue**.

Instead, report it privately using GitHub's [private vulnerability reporting](https://github.com/tstapler/consolette/security/advisories/new) ("Report a vulnerability" under the repo's Security tab).

Include:

- A description of the vulnerability and its impact
- Steps to reproduce, or a proof-of-concept
- The affected version (`consolette --version`)

## Supported Versions

This project is pre-1.0 and does not yet maintain parallel supported release branches — security fixes land on `main` and are included in the next tagged release.
