# Security policy

scull is a tool that deletes files. We treat any way to make it delete, move or expose data it
shouldn't as a security vulnerability, including:

- deleting or quarantining anything classified S3, Git-tracked, or outside the scan root;
- following symlinks, junctions or reparse points out of a scan root;
- executing code from a scanned repository (Git config, hooks, `node_modules/.bin`);
- an MCP client or AI agent approving its own cleanup plan;
- bypassing plan hashing, expiry or revalidation.

## Reporting

Please report privately through GitHub's **Report a vulnerability** (Security tab →
Advisories). Do not open a public issue. We aim to acknowledge within 72 hours.

## Supported versions

scull is pre-release. Only the latest `main` is supported until 1.0.

See `docs/THREAT_MODEL.md` for the threat model.
