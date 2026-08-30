# Contributing to hermes-tui-lab

## Development Setup

```bash
git clone https://github.com/nousresearch/hermes-tui-lab
cd hermes-tui-lab
cargo build
cargo test
```

## Running Tests

```bash
# All tests
cargo test

# Specific test suites
cargo test --test audit_test
cargo test --test state_graph_test
cargo test --test scenario_test

# With output
cargo test -- --nocapture
```

## Code Style

- Follow the existing code style
- Run `cargo fmt` before committing
- Run `cargo clippy` and fix all warnings
- All tests must pass before submitting a PR

## Architecture

The project follows a layered architecture:

```
MCP → Session → TerminalBackend → PTY → ScreenState → SemanticScreen → Audit/Exploration
```

When adding features, maintain this separation. Don't build advanced UX behavior
on top of broken terminal primitives.

## Pull Request Process

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Add tests for new functionality
5. Ensure all tests pass
6. Update documentation if needed
7. Submit a PR

## License

By contributing, you agree that your contributions will be licensed under the
MIT License.
