#!/bin/sh
#
#
TARGET_SERVER=${TARGET_SERVER:-192.168.130.93}
ssh -p 3999 -L 9000:localhost:9000 -L 9001:localhost:9001 -L 9002:localhost:80 root@$TARGET_SERVER -N -f
