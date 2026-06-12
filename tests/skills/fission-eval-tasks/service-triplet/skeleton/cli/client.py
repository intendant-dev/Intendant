#!/usr/bin/env python3
"""Job CLI client. See cli/SPEC.md for the contract."""
import sys


def main(argv):
    if len(argv) < 2:
        print("usage: client.py submit|get|wait API_URL ...", file=sys.stderr)
        return 2
    verb = argv[1]
    if verb in ("submit", "get", "wait"):
        # TODO: implement the verbs per cli/SPEC.md
        print("client.py %s: not implemented" % verb, file=sys.stderr)
        return 2
    print("unknown verb: %s" % verb, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
