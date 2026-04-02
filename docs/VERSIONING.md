# Semantic Versioning Guidelines

This document outlines the semantic versioning rules and version bumping procedures for the Glommio project.

## Semantic Versioning Format

Glommio follows the [Semantic Versioning 2.0.0](https://semver.org/) specification:

```
MAJOR.MINOR.PATCH
```

- **MAJOR**: Incompatible API changes
- **MINOR**: Backward-compatible functionality additions
- **PATCH**: Backward-compatible bug fixes

## Version Bumping Rules

### MAJOR Version (x.0.0)
- Breaking API changes
- Removal of public APIs
- Changes that require users to modify their code
- Changes to the public API that are not backward-compatible

### MINOR Version (x.y.0)
- New public APIs
- New features that are backward-compatible
- Adding new optional features
- Deprecations that are not breaking (with a clear migration path)
- Changes that require users to update to the same major version

### PATCH Version (x.y.z)
- Bug fixes
- Internal implementation changes
- Documentation improvements
- Performance optimizations that do not affect the API

## Version Bumping Procedures

### Automatic Version Bumping

When committing changes, follow these steps:

1. **Check current version** in `glommio/Cargo.toml`
2. **Determine the appropriate bump** based on change type
3. **Update version** in `glommio/Cargo.toml`
4. **Update version** in `README.md`
5. **Commit changes** with appropriate message

### Manual Version Bumping

For complex changes requiring manual versioning:

1. **Update version** in `glommio/Cargo.toml`
2. **Update version** in `README.md`
3. **Update CHANGELOG** if applicable
4. **Tag the release** with the version number
5. **Create a release** on GitHub

## Breaking Change Guidelines

### When to Make Breaking Changes

Breaking changes should only be made when:

1. The change is necessary for the project's stability
2. The deprecation period has been adequately communicated
3. There is a clear migration path for users
4. The change addresses a critical issue or security vulnerability

### Deprecation Guidelines

- **Minimum 2 releases before removal**: APIs should be deprecated for at least two minor versions
- **Document the deprecation**: Include deprecation warnings and migration paths
- **Provide examples**: Show how to migrate from deprecated APIs
- **Maintain compatibility**: Deprecated APIs should continue to work in the current major version

## Version Compatibility

### Backward Compatibility

All changes should maintain backward compatibility unless explicitly breaking. This includes:

- Public API stability
- Behavior compatibility
- Error handling consistency
- Documentation consistency

### Version Compatibility Matrix

For version-specific compatibility information, see [BACKWARD_COMPATIBILITY.md](./BACKWARD_COMPATIBILITY.md).

## Release Process

See [RELEASE_PROCESS.md](./RELEASE_PROCESS.md) for detailed release procedures.

## Tools and Automation

- **cargo-public-api**: Used for API compatibility checking
- **cargo-semver-checks**: Used for semantic versioning validation
- **CI/CD**: Automated version checks in the GitHub Actions workflow

## Common Version Bumping Scenarios

### Scenario 1: Bug Fix Only
```
# Before
version = "0.10.0"

# After
version = "0.10.1"
```

### Scenario 2: New Feature
```
# Before
version = "0.10.0"

# After
version = "0.11.0"
```

### Scenario 3: Breaking Change
```
# Before
version = "0.10.0"

# After
version = "0.11.0"
```

## References

- [Semantic Versioning Specification](https://semver.org/)
- [Semantic Versioning RFC](https://github.com/semver/rfcs/blob/master/02-semver.md)
- [Cargo Versioning](https://doc.rust-lang.org/cargo/reference/manifest.html#the-version-field)
- [Conventional Commits](https://www.conventionalcommits.org/)