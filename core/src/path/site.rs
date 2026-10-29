//! The sites in `crate::path` that lift a ban for one call each: see
//! `lint/site.rs`.  Visible to `crate::path` alone.

use crate::site::Site;

/// Sites that raise a card.
#[derive(Clone, Copy)]
pub(in crate::path) enum Surface {
    /// Creates an absent leaf exclusively beside the directory the walk holds,
    /// so a file that appeared since the walk is refused, never written.  The
    /// write that follows surfaces it; [`crate::own`]'s callers raise none: no
    /// model named the object.
    Create,
    /// The atomic `>` commit step: renames the staged sibling over the
    /// target's name within the one directory handle.  The write surface fires
    /// when the write settles, committed once this returns `Ok`;
    /// [`crate::own`]'s callers raise none: no model named the object.
    #[cfg(unix)]
    Publish,
    /// The one open of a held leaf.  Where the OS can, it is derived from the
    /// leaf's handle so no name is resolved twice; otherwise (macOS, and a
    /// Windows reparse point that names no path) it is opened by name under
    /// the held directory and verified against the identity the walk
    /// recorded, so a name that now leads elsewhere is refused.  Every caller
    /// surfaces it, each in its own way: a redirect's card is fused on by the
    /// frame that wrapped the locate, exarch's readers speak their own card,
    /// and a write's before- and after-images ride the card of the operation
    /// that asked for them.  [`crate::own`]'s callers raise none: no model
    /// named the object.
    Reopen,
    /// The atomic `>` staging create: a fresh exclusive sibling in the
    /// target's own directory, holding the write until the rename publishes
    /// it.  A sub-step of the write site; the reported write is the
    /// operation's surface, and [`crate::own`]'s callers raise none: no model
    /// named the object.
    Stage,
}

impl Site for Surface {}

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(in crate::path) enum Silent {
    /// The host-side size probe of its own scratch and log trees (`tree::bytes`).
    /// It raises no card: no grant judges these resource-accounting reads;
    /// model traversals use the grant's read region instead.
    OwnTreeRegion,
    /// The by-spelling parent in `lex::spelled_parent`: a lexical read of a
    /// name that opens no object, so it raises no card.  It is exempt so that
    /// a program's own layout has one helper to ask, and `Path::parent` stays
    /// banned everywhere else.
    SpelledParent,
    /// A path's parent by spelling, with `.` standing in lexically for an
    /// empty one: a read of names that opens no object, so it raises no card.
    ParentOrCwd,
    /// The one reading of a head that `std::path` finds no prefix in (a
    /// doubled-separator head under Windows rules): a lexical read of names.
    UnparsedHead,
    /// A spelling's names taken relative to a root, the one slash-joined key
    /// over traversal names: lexical, so it opens no object.
    RelativeKey,
    /// The UTF-8 check itself.  The text is the path's native spelling, lifted
    /// back unchanged, so a name no string can hold is refused where it
    /// enters.  A conversion, not an access.
    Utf8,
    /// The `basename` helper the ban names: a string's final component, read
    /// lexically, and the whole string where that name is not UTF-8.  It opens
    /// no object.
    Basename,
    /// An extension read lexically off a string's final name, and a `str`
    /// again since it is a slice of one.  It opens no object.
    Extension,
    /// The proper ancestors of a string, a lexical walk up the name: nothing
    /// is resolved and no object is opened.
    #[cfg(target_os = "macos")]
    ProperAncestors,
    /// Probes each ancestor of the cwd for a `.git` entry at session start,
    /// before any grant governs.  It is ral's own repository discovery, not a
    /// name the model gave, and a stat by name opens nothing, so it raises no
    /// card.
    GitRoot,
    /// The spellings from a work tree's root, already found, down to the cwd:
    /// a lexical read of names that opens no object.
    GitWorkTreeChain,
    /// Asks whether the `.git` entry that session-start discovery found is a
    /// directory or a pointer file.  It is ral's own repository discovery, not
    /// a name the model gave, so it raises no card.
    GitDotGit,
    /// The `which`/PATH probe: stats a candidate to read its executable bit.
    /// An executable-probe predicate, not turn-time model data I/O, so it
    /// raises no card.
    WhichStat,
    /// `PATHEXT`, the Windows resolver's suffix list, which is no XDG base: a
    /// PATH-probe environment read, taken beside the other `which` probes.  It
    /// reads a variable and opens no object, so it raises no card.
    #[cfg(windows)]
    Pathext,
    /// A surface already in normal form, lifted as a `Path`: lexical, with no
    /// I/O behind it.
    SurfaceLift,
    /// A guest path read by the guest's own engine, whose OS spells it as the
    /// guest does: a lexical lift that opens no object.
    GuestNative,
    /// The UTF-8 check a name passes before it is rendered into a sandbox
    /// rule.  A name that is not UTF-8 is refused, never approximated.  A
    /// conversion, not an access.
    #[cfg(unix)]
    RuleText,
    /// The one lossy conversion every shown path goes through.  It is for
    /// display only, so it names no object and raises no card.
    Shown,
    /// Rebases one rendered kernel name onto another, in the one module that
    /// mints them: a lexical read of names that opens no object.
    #[cfg(target_os = "linux")]
    RenderedRebase,
    /// A name that is not UTF-8 is its own collision key, so the check for
    /// UTF-8 is a conversion at the key's edge.  It opens no object.
    #[cfg(any(target_os = "macos", test))]
    CollisionKey,
    /// A path's names, without a head or a root: the one place a component scan
    /// is made for identity.  Lexical.
    NormalNames,
    /// The containment kernel's byte-wise prefix test, over two spellings as
    /// given.  Lexical: it opens no object.
    ContainmentPrefix,
    /// Lifts both strings to paths so the containment kernel can compare them;
    /// nothing is resolved.
    #[cfg(target_os = "linux")]
    ContainmentStr,
    /// The containment kernel's component-wise key: a path's components, each
    /// folded as the collision identity folds it.  Lexical.
    CollisionComponents,
    /// POSIX identity is `Path::starts_with` itself, byte-exact per component,
    /// over two strings lifted to paths.  Lexical: no object is opened.
    PosixIdentity,
    /// A name the wire or the walk holds, lifted to a path for a lexical
    /// split: nothing is resolved.
    LexicalLift,
    /// The one split of a spelling into its root and names.  Lexical: it opens
    /// no object.
    SpellingSplit,
    /// A position's final component, refused where it is not UTF-8: no command
    /// name is one, so a command has no name to speak of there.
    RealName,
    /// The position above a position: the one place a path's parent is taken,
    /// read lexically off a position the walk already reached.
    #[cfg(any(target_os = "macos", windows))]
    RealParent,
    /// The ancestors of a position: the one place a path's ancestors are
    /// walked, read lexically off a position the walk already reached.
    RealAncestors,
    /// Whether a position has a parent is whether it is a root: a lexical read
    /// of a position the walk already reached.
    RealRoot,
    /// The string a caller holds, taken as a spelling to walk.  The walk then
    /// resolves it, so the lift opens nothing.
    StrSpelling,
    /// A glob's literal components, where its walk starts: a lexical read of
    /// the pattern.
    GlobComponents,
    /// The steps a walked name reached, counted from the glob's literal root.
    /// Lexical.
    GlobReach,
    /// A glob component that is not UTF-8 holds no wildcard, so the text check
    /// decides whether it can.  A conversion, not an access.
    GlobWildcard,
    /// A walked name taken relative to the cwd the caller named: lexical.
    WalkedRelative,
    /// The refusal of a name no ral string can hold, and where its longest
    /// UTF-8 spelling stops: a read of a path already in hand, for the error
    /// text.
    Unnameable,
    /// The spellings above a root already walked, whose ignore files discovery
    /// reads.  The files are read through the walk, so this lists names and
    /// opens nothing.
    IgnoreAncestry,
    /// The names of a root already walked, for ignore discovery.  Lexical.
    IgnoreNormals,
    /// Opens a root as a handle, the root every walk descends from: `/` on
    /// Unix, a drive, share or volume root on Windows.  Path resolution, not
    /// the model's data I/O.
    WalkRoot,
    /// Opens one component relative to a held directory and never follows it,
    /// to reach the object a grant will judge: path resolution, not the
    /// model's data I/O.  The card belongs to the site that then opens the
    /// leaf.
    WalkStep,
    /// Reads a link's target through the handle that opened it, to splice into
    /// the remaining name: path resolution, not data I/O.
    #[cfg(target_os = "linux")]
    WalkLinkRead,
    /// Asks the kernel for a held object's name; the walk verifies its identity.
    #[cfg(target_os = "linux")]
    WalkCurrent,
    /// Stats the handle the walk holds, or one entry of a held directory
    /// without following it: a metadata read, never turn-time data I/O.
    #[cfg(target_os = "linux")]
    WalkStat,
    /// Makes an absent entry of a held directory, which the caller judged as
    /// the absent leaf it is; the directory is then stepped onto.
    WalkMkdir,
    /// Rollback of the atomic `>`: unlinks the staged sibling for a write that
    /// will not land.  The aborted write is the surface; this removal raises
    /// none of its own.
    #[cfg(unix)]
    WalkAbandon,
    /// Removes an entry of a held directory by name, never followed: a file,
    /// a link, or a directory the walk emptied.  The program's own
    /// housekeeping, not a model's data I/O, so it raises no card.
    #[cfg(unix)]
    WalkRemove,
    /// Renames an entry of a held directory within it, over the name it
    /// takes: the program's own rotation and quarantine, so it raises no card.
    #[cfg(unix)]
    WalkRename,
    /// Reopens a held directory for reading, which an `O_PATH` or `O_SEARCH`
    /// descriptor cannot do: for a listing and a flush, relative to the
    /// directory itself.
    #[cfg(unix)]
    WalkReopenDir,
    /// Derives another handle from a held Windows object without resolving a
    /// name. Data opens belong to their caller's surface; listing, metadata
    /// and sandbox reopens raise no card of their own.
    #[cfg(windows)]
    WalkReopen,
    /// Whether a write open would be granted, by name under the held
    /// directory: the `is-writable` predicate, and a staged write's check of
    /// the file it would replace.  A permission predicate, not turn-time
    /// model data I/O.
    WalkAccess,
    /// The discard device (`/dev/null`, `\\.\NUL`) opened by its fixed name: no
    /// bytes reach or leave the model, and no grant region can contain a
    /// device that is not a file.
    DiscardDevice,
    /// Asks a held handle for the GUID path of its volume, to say whether the
    /// volume is local: the handle's own answer, so no name is resolved.
    #[cfg(windows)]
    WalkVolume,
    /// Each name of the cwd becomes part of the readable half of a project's
    /// slug, read as text where it is not UTF-8.  The half is for a person
    /// scanning a listing, and the hash beside it carries identity.  It reads
    /// names in hand and opens no object, so it raises no card.
    SlugKey,
}

impl Site for Silent {}
