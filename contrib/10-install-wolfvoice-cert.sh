#!/bin/sh
# certbot DEPLOY hook — publish the cert to the voice service.
#
# The service runs as the unprivileged `wolfvoice` user and therefore cannot read
# /etc/letsencrypt/{live,archive} (root-only, 0700). Rather than loosen those, copy
# the two files it needs into /etc/wolfvoice/tls/ owned by wolfvoice:wolfvoice, key
# mode 0640. Written to a temp name and moved into place so the service never sees
# a half-written key.
#
# RENEWED_LINEAGE is set by certbot to /etc/letsencrypt/live/<name>.
set -eu

: "${RENEWED_LINEAGE:?deploy hook invoked without RENEWED_LINEAGE}"

DEST=/etc/wolfvoice/tls
install -d -o root -g wolfvoice -m 0750 "$DEST"

install -o wolfvoice -g wolfvoice -m 0644 "$RENEWED_LINEAGE/fullchain.pem" "$DEST/.fullchain.pem.new"
install -o wolfvoice -g wolfvoice -m 0640 "$RENEWED_LINEAGE/privkey.pem"   "$DEST/.privkey.pem.new"
mv -f "$DEST/.fullchain.pem.new" "$DEST/fullchain.pem"
mv -f "$DEST/.privkey.pem.new"   "$DEST/privkey.pem"

# Harmless until the unit exists; picks up the new cert once it does.
systemctl reload-or-restart wolfvoice.service 2>/dev/null || true
exit 0
