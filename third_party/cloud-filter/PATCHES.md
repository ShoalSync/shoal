# cloud-filter: Tidefell's patches

This is `cloud-filter` 0.0.6 as published on crates.io. It is commit `1cde645` of
<https://github.com/ho-229/cloud-filter-rs>; upstream `main` adds only example changes since.

Tidefell uses it through `[patch.crates-io]` in the workspace `Cargo.toml`. The copy sits
outside the workspace, so the workspace's `unsafe_code = "forbid"` does not apply to it. The
crate's tests and `Cargo.lock` are left out.

The commit that vendored it changed nothing. Every change since is one of the patches below.
`git log -p -- third_party/cloud-filter` shows them.

Drop this copy, and the `[patch.crates-io]` entry, once a release upstream has the fixes.

Upstream pull request: <https://github.com/ho-229/cloud-filter-rs/pull/11> (2026-10-20). This
copy matches it.

## Handoff: submitting the fixes upstream

This section is for whoever opens the upstream pull request.

**Where to send them:**
- Target ho-229/cloud-filter-rs, branch `main`.
- Each fix below is independent and small. They can be one pull request with one commit per
  fix, or separate pull requests.
- None of the fixes touch the examples.

**No breaking change:** fix 2 keeps `Request::process`'s signature and adds
`Request::try_process`.

**Where it came from:** ok-nick/wincs, the crate `cloud-filter` was forked from, already
reached the design of fixes 1 and 3: its callbacks return `()`, and every ticket has a `fail`.
Mentioning that may help the maintainer accept the change.

**Evidence:**
- Every claim below was observed on Windows 11 25H2, under an ordinary user with no package
  identity.
- It comes from Tidefell's spike: `crates/fs-cfapi/tests/primitives.rs`. Run it with
  `cargo test -p tidefell-fs-cfapi --test primitives -- --ignored --nocapture`.
- The `primitive:` lines it prints are the findings quoted here.
- The spike can be adapted into a test for upstream, which has a `tests/behavior` harness.

### 1. A failed `fail` in a callback aborts the process

`filter/proxy.rs` handles an `Err` from a `SyncFilter` callback by calling `command::X::fail(..)`
and `.unwrap()`ing the result. This happens in six places: `fetch_data`, `validate_data`,
`fetch_placeholders`, `notify_dehydrate`, `notify_delete` and `notify_rename`.

That `fail` itself fails when the platform no longer has the request:
- The fetch timed out, which happens after 60 s without data or progress.
- The user cancelled it.
- In one observed case, it failed for a request that was still live, with
  `HRESULT(0x8007017C)` "The cloud operation is invalid."

The proxies are `extern "system"`, so the panic cannot unwind. The process aborts with
`STATUS_STACK_BUFFER_OVERRUN` (0xc0000409).

Observed:

```
panicked at cloud-filter-0.0.6\src\filter\proxy.rs:97:63:
called `Result::unwrap()` on an `Err` value: Error { code: HRESULT(0x8007017C), message: "The cloud operation is invalid." }
```

The fix ignores the result of `fail`, because nothing more can be done for that request.

### 2. `Request::process` dereferences a null pointer

`CF_CALLBACK_INFO::ProcessInfo` is null for some callbacks, such as `CANCEL_FETCH_DATA`, and
`process()` dereferenced it unconditionally. So did `Request`'s `Debug`, which calls it.

```
panicked at cloud-filter-0.0.6\src\filter\request.rs:38:26:
null pointer dereference occurred
thread caused non-unwinding panic. aborting.
```

The fix keeps `process()`'s signature, so it breaks no caller:
- With no process information, `process()` returns an empty `Process`.
- The accessors check their string pointers.
- The new `try_process() -> Option<Process>` tells that case apart.

### 3. A fetch cannot be failed from outside the callback

`ticket::FetchData` had no `fail`. Returning `Err` from the callback was the only way to fail a
fetch, so a provider could not answer the callback at once and fetch on its own thread.

That matters for two reasons:
- **Callbacks can hold each other up.** Three slow fetches in three sync roots of one process
  did not run side by side: two finished 60 s after the first.
- **Sending nothing is not a failure.** A fetch answered with no data leaves the reading
  program waiting for the platform's timeout and one retry. That was 241 s, then
  `ERROR_CLOUD_FILE_REQUEST_TIMEOUT` (426).

The fix adds `FetchData::fail(&self, CloudErrorKind) -> core::Result<()>`, using the same
`command::Write::fail` the proxy uses. `FetchData` holds only the two keys, so it can already
move to another thread.

### 3b. Failing a fetch of nothing is refused

`command::Write::fail` sends `TRANSFER_DATA` with `Offset: 0, Length: 0`. The source has a
`TODO: DOES THIS HAVE TO BE DEFINED?` there.

The platform refuses it with `HRESULT(0x8007017C)` "The cloud operation is invalid." The
reading program then waits for the timeout:

```
fail now: read after 301.9s: Err(Os { code: 426, .. "The cloud operation was not completed before the time-out period expired." })
spawned: answered after 5s: Err(Error { code: HRESULT(0x8007017C), message: "The cloud operation is invalid." })
```

That happened in both cases: an `Err` returned from the callback, and `fail` from another
thread.

The fix adds `Write::fail_range`, which fails the range the platform asked for, as Microsoft's
CloudMirror sample does. The fetch proxy and `FetchData::fail` use it, and the ticket keeps the
required range for that.

With the fix, both ways of failing work:

```
fail now: read after 3.9ms: Err(Os { code: 388, .. "The cloud sync provider failed to perform the operation due to network being unavailable." })
spawned fail: read after 10.0s: Err(Os { code: 388, .. })
```

The 10 s is two 5 s fetches, the original and the one retry the platform makes after a
failure.

Answering from other threads also shows that the platform runs about two fetches of one
process at once. Three 5 s fetches answered from their own threads finished at 5.0 s, 5.0 s and
10.1 s.

### 4. USNs are read and cast to pointers

`Placeholder::mark_in_sync` and `Placeholder::convert_to_placeholder` passed
`usn.into().map(|x| ptr::read(x) as *mut _)`. That reads the USN's value and uses it as a
pointer.

The fix passes `x as *mut _`, as `Placeholder::update` already does. Passing `None`, as
Tidefell does, was never affected.

### 5. Ranges to dehydrate are passed only when there are none

`Placeholder::update` passed `dehydrate_ranges` when the list was empty, and nothing when it had
ranges. The condition was inverted.

The fix negates the condition. Dehydrating a whole file with `UpdateOptions::dehydrate()` and no
ranges works either way, and the spike confirmed it.

### 6. `Placeholder::hydrate` passes an end where the API takes a length

`CfHydratePlaceholder(handle, StartingOffset, Length, ..)` takes a length. The method passed the
range's end, or `end - 1` for an exclusive end.

The fix computes `end - start`, and keeps `-1`, the rest of the file, for an unbounded end.
`hydrate(..)` from the start of a file was never affected.

### 7. `Placeholder::info` panics on a folder placeholder

`info()` truncates its buffer to the size the platform reports, then slices `..=size_of` of the
info structure. That is one byte past the structure. For a folder placeholder without an
identity blob, the platform reports 60 bytes:

```
panicked at src\placeholder.rs:712:29:
range end index 64 out of range for slice of length 60
```

`fixed_size_info` slices the same way, so it panics for a blob size of 0.

The fix keeps at least the structure's size, which is zeroed past what the platform wrote, and
slices `..size_of`.

Not yet in the upstream pull request (found 2026-10-20, after it was opened).

## Also observed, not patched

These are useful context for upstream, but they are not bugs in the crate:
- Operations a provider makes in its own process raise no callbacks: its own deletes and
  renames in the sync root, for example.
- `std::fs::metadata`, which follows the placeholder, does not show
  `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`. `symlink_metadata` does.
- `convert_to_placeholder` and `update` need a handle opened with
  `Placeholder::options().write_access().exclusive()`. With `Placeholder::options()` alone they
  fail with `HRESULT(0x80070006)` "The handle is invalid."
- Renaming an ordinary file over a dehydrated placeholder first hydrates the old placeholder,
  through `fetch_data`.
- Reporting progress during a fetch, or progressive hydration, keeps a fetch alive past 60 s.
- Another process renaming a plain folder of placeholders raises no callback. Renaming a
  placeholder folder raises one `rename` and one `renamed` for the folder.
- A placeholder moved into a hidden folder in the root, or out of the root, stays a placeholder,
  but is no longer in sync. Dehydrating it is then refused: "The file is not in sync with the
  cloud", or, out of the root, "only supported on files under a cloud sync root".
- `PlaceholderFile` refuses to create a folder placeholder with `has_no_children` and
  `mark_in_sync`. Converting a plain folder works.
