#!/bin/sh
# certbot POST hook — close the ACME http-01 window.
#
# Runs whether the renewal succeeded or failed, so the window never stays open
# after a failed attempt. `delete element` errors if the element is absent (e.g.
# the pre hook never ran), which must not be treated as a failure — hence the
# guard rather than `set -e` alone.
set -u
/usr/sbin/nft delete element inet filter certbot_open '{ 80 }' 2>/dev/null || true
exit 0
