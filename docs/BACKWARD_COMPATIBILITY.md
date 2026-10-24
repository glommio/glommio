# Backward Compatibility Guidelines

This document provides detailed guidelines for maintaining backward compatibility in the Glommio project and managing API changes.

## API Change Process

### 1. Planning API Changes

Before making any API changes:

1. **Review the API**: Understand the current public API surface
2. **Consider impact**: Analyze how the change affects existing users
3. **Plan for compatibility**: Design the change to be backward-compatible if possible
4. **Document the change**: Prepare clear documentation for the change

### 2. Making Non-Breaking Changes

When adding new features:

- Add new public APIs without modifying existing ones
- Add new modules and types
- Add new functions and methods
- Keep existing APIs stable

### 3. Making Breaking Changes

When making breaking changes:

1. **Deprecate first**: Mark the API as deprecated with a clear deprecation warning
2. **Provide migration path**: Document how to migrate from the old API
3. **Wait for next major version**: Don't remove the API until the next major release
4. **Update documentation**: Ensure all references to the deprecated API are updated

## Deprecation Guidelines

### Deprecation Warnings

Deprecation warnings should:

- Be clear and informative
- Include a suggested replacement
- Show code examples of the old vs new approach
- Be visible in documentation

### Deprecation Timeline

- **Initial deprecation**: First mention in documentation
- **First warning**: Compile-time warning in version N
- **Second warning**: Stronger compile-time warning in version N+1
- **Removal**: After version N+2

Example deprecation format:

```rust
/// # Deprecated
///
/// This function is deprecated and will be removed in version 0.12.0.
///
/// Please use [`new_function()`](Self::new_function) instead.
#[deprecated(since = "0.10.0", note = "Use `new_function()` instead")]
pub fn old_function() {
    // implementation
}
```

## Migration Paths

### For API Users

When encountering a deprecation:

1. **Update code**: Replace deprecated API with new API
2. **Test thoroughly**: Ensure the new API works as expected
3. **Report issues**: If the migration is difficult, report the issue

### For Library Authors

When updating to use a deprecated API:

1. **Plan the migration**: Determine when to update
2. **Update incrementally**: Update one module at a time
3. **Maintain compatibility**: Don't break users until ready

## Version Compatibility Rules

### MAJOR Version Changes

- Only for breaking changes
- Requires user code updates
- Can include API additions and removals
- Can include behavior changes

### MINOR Version Changes

- For new features and API additions
- Must maintain backward compatibility
- Can include deprecations
- Can include bug fixes

### PATCH Version Changes

- For bug fixes only
- Must maintain all API compatibility
- No new features
- No breaking changes

## Testing Backward Compatibility

### API Stability Testing

- Run existing tests to ensure nothing is broken
- Test with various Rust versions if applicable
- Test with different feature combinations

### API Compatibility Tests

Use the automated tests in `glommio/tests/public_api.rs` to:

- Check for unintended API changes
- Ensure no public APIs are removed without notice
- Validate backward compatibility

## Common Pitfalls

### Accidental Breaking Changes

- **Renaming**: Don't rename public APIs without deprecation
- **Changing signatures**: Don't change function signatures
- **Removing parameters**: Don't remove parameters from functions
- **Changing behavior**: Don't change behavior without documentation

### Dependency Issues

- **Breaking dependencies**: Don't update dependencies in a breaking way
- **API changes in dependencies**: Don't rely on internal APIs of dependencies
- **Version constraints**: Be careful with dependency version constraints

## Documentation Requirements

### API Documentation

All public APIs must have:

- Clear function signatures
- Parameter descriptions
- Return value descriptions
- Error documentation
- Usage examples

### Deprecation Documentation

Deprecated APIs must have:

- Deprecation notice
- Suggested replacement
- Migration guide
- Removal timeline

## Release Notes

Release notes should:

- List all breaking changes
- Document deprecations
- List new features
- Document bug fixes
- Provide migration instructions for breaking changes

## Examples

### Example 1: Adding a New Function

```rust
/// Adds a new feature without breaking compatibility
pub fn new_feature() -> Result<(), Error> {
    Ok(())
}
```

### Example 2: Deprecating an Old Function

```rust
/// # Deprecated
///
/// This function is deprecated and will be removed in version 0.12.0.
///
/// Please use [`new_function()`](Self::new_function) instead.
#[deprecated(since = "0.10.0", note = "Use `new_function()` instead")]
pub fn old_function() {
    // implementation
}

/// The new replacement function
pub fn new_function() {
    // implementation
}
```

### Example 3: Changing Internal Implementation

```rust
/// Internal helper function
/// This is private, so changing it doesn't affect users
fn internal_helper() {
    // implementation
}
```

## References

- [Semantic Versioning](https://semver.org/)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- [Deprecation in Rust](https://doc.rust-lang.org/stable/reference/deprecations.html)