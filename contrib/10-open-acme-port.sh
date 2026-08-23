#!/bin/sh
# certbot PRE hook — open the ACME http-01 window.
#
# The firewall (/etc/nftables.conf) keeps 80/tcp shut and permits it only via the
# named set `certbot_open`, so this is the entire mechanism by which Let's Encrypt
# can reach us. The matching post hook removes the element again; the set is empty
# in the on-disk ruleset, so a reboot mid-renewal also closes it.
set -eu
/usr/sbin/nft add element inet filter certbot_open '{ 80 }'
