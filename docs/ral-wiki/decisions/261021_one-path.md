---
status: proposed
generated_at_commit: 6f703454
---

# One path

**Every path ral judges is the path of an object its own walk opened, and
every operation on it is relative to the handle that walk holds.** The
principle is stated in §1–2 of `dev/docs/plans/261021_one-path.md`: a name is
resolved once, by `path/walk.rs`, through directory handles and never by the
kernel's following; what the grant judges is the transcript of that walk, and
what is then read or written is the object it acquired. The decision body is
written when the plan lands, in its last step.

## Verification

The macOS and Windows code of this commit — the `macos` and `windows` host
modules of `path/walk.rs` (step 1) and their tests — was cross-compiled only,
on a Linux host: `just check-macos`, and `just check-windows` at full
workspace scope. Neither has run on its host. The host-assertion tests that
have not, by name:

- macOS (`path/walk/tests/macos.rs`):
  `a_case_variant_is_recorded_at_the_spelling_the_directory_stores`,
  `the_stored_name_rule_takes_a_spelling_and_lists_for_another_links_name`,
  `a_hard_link_is_recorded_under_the_name_it_was_opened_by` (the APFS
  hard-link experiment the plan's §9 item 39 assigns to this step, and the
  one that asserts the packing of `getattrlistat` and `getattrlistbulk`
  answers),
  `a_component_that_moved_before_its_name_was_checked_is_refused`,
  `a_link_is_read_through_its_o_symlink_descriptor`,
  `a_file_the_user_cannot_read_is_stated_and_never_opened`,
  `a_walk_through_a_firmlink_records_the_identity_openat_reached`,
  `a_leaf_link_is_held_under_every_mode`,
  `a_dangling_link_under_follow_is_a_create_target_with_no_real_path`; and
  the Unix set of `path/walk/tests/unix.rs` that is not gated to Linux,
  `a_search_only_ancestor_is_walked` among it, which has not run on macOS
  against the new engine.
- Windows (`path/walk/tests/windows.rs`):
  `an_identity_is_the_file_id_of_the_handle`,
  `file_normalized_name_info_answers_the_stored_case`,
  `a_held_prefix_refuses_a_rename_while_it_lives` (the no-delete-share
  premise),
  `a_listing_without_128_bit_ids_falls_back_to_the_64_bit_id` (gated on
  `RAL_TEST_FAT_VOLUME`, the path of a directory on a FAT VHD, and skipped
  with its reason where unset),
  `a_stat_carries_the_length`,
  `a_reparse_buffer_decodes_to_the_target_it_names`,
  `a_reparse_point_that_names_no_path_is_no_link`,
  `a_component_renamed_is_adopted_and_one_moved_is_refused`,
  `publish_through_file_rename_info_ex_lands_the_file` (the plan's §9 item 3
  pin: the held `RootDirectory`), and the three converted tests,
  `existing_and_absent_leaves_use_the_parents_stored_case`,
  `a_walk_starts_only_from_a_parsed_root` and
  `a_file_reached_by_its_short_alias_locates_at_its_long_name`.

Step 3 (the layer, part a) adds no host-assertion test of its own: its
macOS and Windows changes — the Seatbelt and AppContainer renderers reading
the resolved exec and fs rules, `FreezeCtx`'s home walk, the floor's walk of
`SystemRoot` and `ProgramFiles` — were cross-compiled only, and the layer
tests it ports run on those hosts under the same first green run.

Step 4 (who vouched, part a) is likewise cross-compiled only where it is not
Linux: on Windows, `windows/session.rs::listing`, the carving lister, now
holds each directory by `RealPath::open_dir_nofollow` as `Installing` and
lists it through `Dir::read_dir`, and the host's `reopen_shared` and
`is_local` (`GetFinalPathNameByHandleW` and `GetDriveTypeW`) behind the
`Installing` doors `reopen` and `is_local` are written and unused until the
sandbox's step; on macOS, the pin of ral's own binary (`reexec.rs`) is by
`ObjectId`, which `From<&std::fs::Metadata>` makes of the by-name stat
`verify` compares.  `dacl/frozen.rs` and `dacl/deny.rs` open by name as
before.  Their tests run on those hosts under the same first green run.

Step 7 (the sandbox, mechanically), on Windows, is cross-compiled only.
`carve` runs over a lister that opens each directory it lists as
`Dir<Installing>` (`windows/session.rs::listing`); `FrozenPin::open` holds its
position by `open_nofollow`, refuses a share by `Located::is_local` and
derives its stamping handle by `reopen`; `ensure_unreachable` holds each
exclusion, a file or a directory, and reads every descriptor through
`reopen(READ_CONTROL)`; the by-name opens, `ensure_local_canonical_prefix` and
`dacl/acl.rs`'s named route are gone.  The `FileRenameInfoEx` premise with a
non-null `RootDirectory` (plan §9 item 3) is pinned by
`publish_through_file_rename_info_ex_lands_the_file` (step 1) and is not
confirmed: the redirect commit's Windows publish rests on it until that test
has run on a host.  The step-7 host assertions that have not run:
`an_exact_file_exclusion_is_verified_and_reported` and
`a_junction_and_a_symlink_beneath_an_exclusion_are_not_followed`
(`dacl/tests.rs`), `a_share_is_refused` and the junction cases
(`dacl/frozen/tests.rs`), and `carving_meets_a_junction_and_a_symlink_as_links`
(`windows/session/tests.rs`).

Step 7 (the sandbox, mechanically), on macOS, is cross-compiled only.
The pin of ral's own binary keeps the `Trace` of the walk that found it
(`Pinned::open`), and `reexec::OWN` is the one hold on it: `ral_binary()`, the
launch's Windows image and `verify` read it, and `verify` walks the pinned
name again and compares the leaf's `ObjectId` with the pin's, where it
re-`stat`ted by name before.  The Seatbelt profile's pins are
`RealPath::ancestors_within` of the positions each exclusion holds, rendered
with their firmlink twins, and an exclusion renders from those positions, its
name's and its link's target's, never from the text as written; `shims`
compares `program.parent()` by `RealPath::parent`.  No macOS host assertion
is new: `mac_profile_lets_a_lookup_stat_the_surface_of_a_frozen_allowance`
and the profile tests that build exclusions (now `GrantPath<Excluded>`, so
walked) are the existing ones, unrun on a Mac.

The first green macOS `ci.yml` and `windows.yml` run on the landing commit
discharges them; it is recorded here when it comes.
