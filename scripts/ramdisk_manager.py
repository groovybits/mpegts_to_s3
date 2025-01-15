#!/usr/bin/env python3
import argparse
import subprocess
import sys

def setup_ramdisk(path, size_mb):
    """
    Mount a tmpfs at `path` of size `size_mb` (in MB).
    Requires sudo privileges.
    """
    try:
        subprocess.run(
            [
                "sudo", 
                "mount", 
                "-t", "tmpfs", 
                "-o", f"size={size_mb}m", 
                "tmpfs", 
                path
            ],
            check=True
        )
        print(f"RAM disk set up at {path} with size {size_mb} MB")
    except subprocess.CalledProcessError as e:
        print(f"Failed to set up RAM disk at {path}: {e}")
        sys.exit(1)


def teardown_ramdisk(path):
    """
    Unmount the tmpfs at `path`.
    Requires sudo privileges.
    """
    try:
        subprocess.run(["sudo", "umount", path], check=True)
        print(f"RAM disk at {path} torn down")
    except subprocess.CalledProcessError as e:
        print(f"Failed to tear down RAM disk at {path}: {e}")
        sys.exit(1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Manage a RAM disk via tmpfs (setup or teardown)."
    )
    parser.add_argument(
        "action",
        choices=["setup", "teardown"],
        help="Action to perform: set up or tear down the RAM disk"
    )
    parser.add_argument(
        "--path",
        default="/mnt/ramdisk",
        help="Path to mount or unmount"
    )
    parser.add_argument(
        "--size_mb",
        default=1024,
        type=int,
        help="Size of the RAM disk in MB (only used if setting up)"
    )
    args = parser.parse_args()

    if args.action == "setup":
        setup_ramdisk(args.path, args.size_mb)
    elif args.action == "teardown":
        teardown_ramdisk(args.path)
