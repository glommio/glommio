# Version Release Checklist

This checklist ensures all necessary steps are completed when releasing a new version of Glommio.

## Pre-Release Checklist

### 1. Version Planning
- [ ] Identify version type (MAJOR, MINOR, PATCH)
- [ ] Update version in `glommio/Cargo.toml`
- [ ] Update version in `README.md`
- [ ] Review CHANGELOG for relevant entries
- [ ] Confirm all changes are documented

### 2. Testing
- [ ] Run full test suite: `cargo test`
- [ ] Run API compatibility tests: `cargo test --package glommio public_api`
- [ ] Update API snapshot if needed: `UPDATE_SNAPSHOTS=yes cargo test --package glommio public_api`
- [ ] Run semantic versioning checks: `cargo semver-checks --package glommio`
- [ ] Run linters: `cargo clippy --all-targets --all-features`
- [ ] Check formatting: `cargo fmt --all -- --check`
- [ ] Verify documentation generation: `cargo doc --package glommio`
- [ ] Validate documentation links: `cargo deadlinks --dir target/doc/glommio`

### 3. Documentation
- [ ] Update README with new version information
- [ ] Update any relevant API documentation
- [ ] Document deprecations if applicable
- [ ] Update migration guides for breaking changes
- [ ] Verify all examples compile and work

### 4. Release Preparation
- [ ] Update CHANGELOG.md with release notes
- [ ] Create release branch if needed
- [ ] Tag the release: `git tag -a v{VERSION} -m "Release v{VERSION}"`
- [ ] Push tag to repository: `git push origin v{VERSION}`

## Release Day Checklist

### 1. Final Verification
- [ ] Verify all CI tests pass
- [ ] Verify semantic versioning checks pass
- [ ] Verify API compatibility tests pass
- [ ] Review release notes for completeness
- [ ] Check for any warnings or issues in logs

### 2. Release Creation
- [ ] Create GitHub Release from the tag
- [ ] Upload release artifacts (if applicable)
- [ ] Add release notes
- [ ] Mark release as production-ready

### 3. Post-Release
- [ ] Update website/documentation links
- [ ] Notify team of release
- [ ] Monitor for issues in the release
- [ ] Begin planning next version

## Version-Specific Checks

### MAJOR Version Release
- [ ] Confirm all breaking changes are documented
- [ ] Provide migration guide for all breaking changes
- [ ] Update all dependent documentation
- [ ] Consider deprecation removal
- [ ] Update migration guides

### MINOR Version Release
- [ ] Document all new features
- [ ] Provide usage examples for new APIs
- [ ] Update migration guides if needed
- [ ] Verify all new features work as expected

### PATCH Version Release
- [ ] Verify all bug fixes work as expected
- [ ] Check for regression in existing features
- [ ] Update CHANGELOG with bug fix entries
- [ ] Verify no breaking changes introduced

## Common Issues to Watch For

- **API Changes**: Ensure no unintended public API changes
- **Dependency Updates**: Verify no breaking dependency changes
- **Documentation**: Check for broken links or outdated information
- **Examples**: Ensure all examples compile and run correctly
- **Tests**: Verify all tests pass on the release version

## Release Template

### Release Notes Template

```
# Release v{VERSION}

## Summary
{Brief description of what's new in this release}

## Breaking Changes
{List of breaking changes with migration instructions}

## New Features
{List of new features with examples}

## Bug Fixes
{List of bug fixes}

## Deprecations
{List of deprecated APIs with migration information}

## Migration Guide
{Detailed migration guide for breaking changes}

## Changed
{List of notable changes}
```

## Tools and Commands

### Common Commands
```bash
# Check version
grep '^version = ' glommio/Cargo.toml

# Run tests
cargo test

# Run API tests
cargo test --package glommio public_api

# Run semver checks
cargo semver-checks --package glommio

# Generate documentation
cargo doc --package glommio

# Validate documentation links
cargo deadlinks --dir target/doc/glommio

# Create release tag
git tag -a v{VERSION} -m "Release v{VERSION}"

# Push release
git push origin v{VERSION}
```

## Contact

For questions or issues with the release process, contact the maintainers.

## References

- [Semantic Versioning](https://semver.org/)
- [BACKWARD_COMPATIBILITY.md](./BACKWARD_COMPATIBILITY.md)