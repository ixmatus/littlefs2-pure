# Glossary

The LittleFS v2 vocabulary the registry entries and the codebase assume. Definitions are this project's own wording; the normative source is the pinned SPEC.md (see `spec-littlefs-v2`).

**Superblock.** The entry at id 0 of the root metadata pair carrying the magic string `littlefs`, the on disk version, the geometry (block size and count), and the name and inline size limits. Mount starts by locating and validating it.

**Metadata pair.** Two flash blocks that together store one logical metadata log. Writes append commits to the active block; when full, the content compacts onto the other block with a bumped revision counter. The pair is the unit of atomic metadata update.

**Revision counter.** A u32 at the head of each block in a pair. The active block is the one whose counter is higher under modular comparison (wrapping subtraction reinterpreted as a signed sign check), so the ordering survives the wrap at 2^32.

**Commit.** A run of tags appended to a metadata block, terminated by a CRC tag. A commit is durable once its CCRC verifies; everything after a failed CCRC is ignored.

**Tag.** The 32 bit header of every metadata entry: valid bit, type, id, length. Tags are stored XOR encoded against the previous tag in the block (the first against all ones), so freshly erased `0xFF` regions decode to a value whose valid bit reads as "no more tags."

**CCRC.** The commit CRC tag (`0x5xx` family) closing a commit. It covers the commit's bytes and decides commit acceptance.

**FCRC.** The erase state checksum (`0x5ff`, disk version 2.1) recording the CRC of the program window that follows a commit, so a reader can tell whether that window is genuinely erased and an append in place is safe. It never moves the accepted commit boundary (see `failure-fcrc-rollback`).

**Inline file.** A file small enough to live entirely inside its parent's metadata pair as an INLINESTRUCT tag, occupying no data blocks.

**CTZ skip list.** The structure of a non inline file: a backward linked list of data blocks where block n stores pointers to blocks n minus 1, n minus 2, n minus 4, and so on (one pointer per trailing zero bit of n, hence the count trailing zeros name), giving logarithmic backward traversal from the head.

**gstate (global state).** Distributed filesystem wide state XORed across deltas carried in metadata commits; the mount time fold of all per pair contributions. LittleFS uses it to make cross pair operations (rename, orphan tracking) power loss atomic.

**MOVESTATE.** The gstate flavor (`0x7ff`) marking a file mid move, so an interrupted rename is detected and resolved at the next mount instead of duplicating or losing the entry.

**Orphan.** A metadata pair reachable through the directory tail list but no longer referenced by any directory entry, left behind by an interrupted operation; the deorphan sweep reclaims them.

**SoftTail.** A tail tag (`0x600`) threading the global linear list of all metadata pairs used by mount scans and the allocator; it does not imply directory membership.

**HardTail.** A tail tag (`0x601`) chaining a directory onto a continuation pair when its entries no longer fit in one pair; the continuation is part of the same logical directory.

**Splice.** The id renumbering performed inside a metadata log when CREATE and DELETE tags shift the ids of later entries; readers must resolve a name through the splice history, not by raw id.

**Compaction.** Rewriting a metadata pair's live content onto its other block, dropping superseded and deleted tags, with the revision counter bumped. Triggered by a full block or by wear policy.

**Relocation.** Moving a metadata pair to fresh blocks, either proactively for wear leveling or on program and erase failures (bad blocks), with the parent's pointer updated afterward.

**Directory splitting.** Allocating a HardTail continuation pair when a directory outgrows one metadata pair, and rebalancing entries across the chain.

**Lookahead.** The allocator's bitmap window over the block space, populated by scanning the reachable filesystem, from which free blocks are handed out.
