# Security policy

## Supported versions

Orqen is pre-1.0. Security fixes go into the latest release only.

| Version | Supported |
| --- | --- |
| 0.1.x | Yes |

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: open the repository's **Security** tab and choose **Report a vulnerability**. Do not open a public issue or pull request.

Include what you can of the following:

- the affected version or commit
- steps to reproduce, or a proof of concept
- the impact you expect, for example key disclosure or request smuggling

You can expect an acknowledgement within 7 days. We will keep you informed while we work on a fix and credit you in the release notes unless you prefer otherwise.

## Scope

Orqen handles provider API keys, so these areas matter most:

- disclosure of key values through logs, errors, `/healthz` or response headers
- forwarding client credentials upstream, or forwarding a pooled key to the wrong host
- binding to a non-loopback address despite the loopback-only rule
- request or response handling that lets one conversation read another's data

Weaknesses in a model provider's own service are out of scope; report those to the provider.
