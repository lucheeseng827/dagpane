#!/bin/sh
# Pull an app directory out of object storage into a local snapshot. One job, no serving.
#
#   ./sync.sh s3://my-bucket/panels/sales            ./snapshot
#   ./sync.sh gs://my-bucket/panels/sales            ./snapshot
#   ./sync.sh az://my-container/panels/sales         ./snapshot
#   ./sync.sh rclone:myremote:my-bucket/panels/sales ./snapshot
#   ./sync.sh ./some/local/dir                       ./snapshot   # for trying this out
#
# The source prefix must contain the manifest and every CSV it names, because a manifest's
# paths resolve against its own directory — so the directory, not the file, is the unit that
# travels. Upload it whole and this stays true on both ends.
#
# Credentials are whatever the underlying CLI already reads: an instance or workload identity,
# a profile, the ambient environment. Nothing here takes a key, and nothing here should be
# given one — if this is running in the cloud at all, attach a role to it and let the tool
# below find it.
#
# EVERY branch below mirrors: a file removed upstream is removed here too. That is the
# property `serve.sh` relies on — a stale CSV nobody meant to keep serving is exactly the
# failure this exists to prevent — so the two branches whose tool has no delete flag
# (`az`, and a local directory) download into a fresh directory and replace the destination
# only on success, rather than copying over the top of whatever was already there.
#
# Exercised in this repository: the local-directory branch, end to end, by the bucket
# example's own walkthrough. The four cloud branches are the documented invocation of each
# vendor's own mirror command and are not run by any test here.
set -eu

SRC="${1:?usage: sync.sh <bucket-uri> <dest-dir>}"
DEST="${2:?usage: sync.sh <bucket-uri> <dest-dir>}"

mkdir -p "$DEST"

# Populate a fresh directory, then swap it in. Used by the branches whose tool cannot delete
# destination-only files on its own. The destination is left untouched if the transfer fails.
mirror_via_swap() {
  tmp="$DEST.tmp.$$"
  rm -rf "$tmp"
  mkdir -p "$tmp"
  if ! "$@"; then
    rm -rf "$tmp"
    echo "sync.sh: transfer failed; leaving $DEST as it was" >&2
    return 1
  fi
  rm -rf "$DEST"
  mv "$tmp" "$DEST"
}

case "$SRC" in
  s3://*)
    # --delete so a file removed upstream is removed here. A stale CSV nobody meant to keep
    # serving is the failure this flag exists to prevent.
    exec aws s3 sync "$SRC" "$DEST" --delete --only-show-errors
    ;;
  gs://*)
    if command -v gcloud >/dev/null 2>&1; then
      exec gcloud storage rsync "$SRC" "$DEST" --recursive --delete-unmatched-destination-objects
    fi
    exec gsutil -q rsync -r -d "$SRC" "$DEST"
    ;;
  az://*)
    # az://<container>/<prefix>
    rest="${SRC#az://}"
    container="${rest%%/*}"
    prefix="${rest#"$container"}"
    prefix="${prefix#/}"
    # `download-batch` has no delete flag, so mirror by swap. It also preserves the blob's
    # full path under the destination, so with a prefix the files land at `<dest>/<prefix>/`
    # rather than at `<dest>/` — the manifest has to end up beside its CSVs at the top, so
    # lift them afterwards.
    az_fetch() {
      az storage blob download-batch \
        --source "$container" ${prefix:+--pattern "$prefix/*"} --destination "$tmp" --no-progress
    }
    mirror_via_swap az_fetch
    if [ -n "$prefix" ] && [ -d "$DEST/$prefix" ]; then
      inner="$DEST/$prefix"
      lifted="$DEST.lift.$$"
      mv "$inner" "$lifted"
      rm -rf "$DEST"
      mv "$lifted" "$DEST"
    fi
    ;;
  rclone:*)
    exec rclone sync "${SRC#rclone:}" "$DEST" --quiet
    ;;
  *)
    [ -d "$SRC" ] || { echo "sync.sh: $SRC is not a directory and not a supported bucket URI" >&2; exit 2; }
    # `cp -R` copies over the top and leaves anything the source no longer has, so mirror by
    # swap here too. Without this the local branch behaves differently from every cloud one,
    # which is worse than it sounds: the local branch is how people try this out.
    local_fetch() { cp -R "$SRC/." "$tmp/"; }
    mirror_via_swap local_fetch
    ;;
esac
