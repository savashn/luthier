# Exit codes

`luthier` returns a predictable status so scripts can react to *why* something
failed, not just that it did.

| Code | Meaning | Typical cause |
|---|---|---|
| 0 | Success | |
| 1 | Generic error | I/O failure, unreadable state, unexpected condition |
| 2 | Invalid arguments | Malformed package ID or version; confirmation needed but stdin is not a terminal |
| 3 | Package not found | No such package in any configured registry |
| 4 | Verification failure | Downloaded artifact did not match its checksum; installed files no longer match what was recorded |
| 5 | Installation failure | Unsafe archive, failed plugin validation, a conflicting unmanaged file |
| 6 | Dependency resolution failure | Cycle, version conflict, missing external dependency, or removal blocked because another package still needs it |

Codes are produced by `Error::exit_code` in `crates/luthier-core/src/error.rs`; a
unit test there asserts this table.

## Notes

**4 vs 5.** Code 4 means *we did not trust what we got*. Nothing was installed
and nothing was changed. Code 5 means the artifact was trustworthy but could not
be installed — for example because an unmanaged plugin already occupies the
destination.

**6 on removal.** Refusing to remove a package another package still depends on
is a dependency failure, so it shares code 6 with resolution failures rather
than collapsing into the generic 1.
