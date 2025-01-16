#!/bin/sh
#
#
ssh -p 3999 -L 9000:localhost:9000 -L 9001:localhost:9001 root@192.168.130.93 -N -f
