# Contributing to whatsrust

Thanks for your interest. Here's how to contribute:

## Process

1. **Fork** the repo and create a branch from `main`
2. **Make your changes** -- add tests for new functionality
3. **Run checks** before submitting:
   ```bash
   cargo test
   cargo clippy
   ```
4. **Open a PR** with a clear description of what changed and why

## Guidelines

- Keep PRs focused. One feature or fix per PR.
- Follow existing code patterns. When in doubt, look at how similar features are implemented.
- Add tests for new outbound operations, API endpoints, and storage methods.
- Don't introduce new dependencies without discussion.

## Questions?

Open an issue. We're happy to help.
