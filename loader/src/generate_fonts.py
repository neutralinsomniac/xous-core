#!/usr/bin/python3

import argparse
import subprocess
import os
import sys
import re
from collections import OrderedDict

"""
This script should be run whenever the fonts are updated inside the blitstr dependency. The
thought is that this is a rare event, and therefore it's better to run this on the rare occassions
that this happens and vendor in the font files as references to their location in the FLASH
memory map.

The main motivation for this is to disintegrate the font maps from the kernel image itself,
as it is a large ball of static data that rarely changes, and it slows down updates and
development. Maybe once the kernel supports servers that are bootable from disk images, we
can do away with this methodology. But for now, this helps keep the kernel trim, and speeds
up the development process.

This script is designed to be run "in-place" where it is found in the xous-core tree. It
hard-codes the location of the graphics-server crate to encode the locations of the fonts.
It also, by default, assumes that the `blitstr` crate is cloned into a directory at the
same level as xous-core, but this can be changed with the `-d` command line argument.
"""
def main():
    parser = argparse.ArgumentParser(description="Build the Betrusted SoC")
    parser.add_argument(
        "-d", "--dir", default="../../../blitstr", help="Location of the blitstr source files", type=str
    )
    args = parser.parse_args()

    fontdir = args.dir + '/src/fonts'
    fontdict = OrderedDict([]) # we want a deterministic dict
    filter = re.compile('.*DATA.*u32.*[0-9]*.*')
    with os.scandir(fontdir) as listOfEntries:
        for entry in listOfEntries:
            if entry.is_file():
                print("Processing " + entry.name)
                modulename = entry.name.split('.')[0]
                with open(entry) as infile, open('fonts/' + entry.name, 'w') as outfile:
                    outfile.write(
                        "// This file is autogenerated by xous-core/loader/src/generate_fonts.py. Do not edit.\n")
                    outfile.write("#[allow(dead_code)]\n")
                    outfile.write("#[link_section=\".fontdata\"]\n")
                    outfile.write("#[no_mangle]\n")
                    outfile.write("#[used]\n")
                    copy = False
                    for line in infile:
                        if line.strip() == "/// Packed glyph pattern data.":
                            copy = True
                        if copy:
                            fixup = line.replace('pub const', 'pub static')
                            matched = filter.match(fixup)
                            if matched:
                                arraylen = re.findall('\d+', matched.group().split(';')[1])[0]
                                fontdict[modulename] = arraylen
                                fixup = fixup.replace('DATA', 'DATA_' + modulename.upper())
                            outfile.write(fixup)
                        if line.strip() == "];":
                            copy = False
    print(fontdict)
    with open('fonts.rs', 'w') as modfile:
        modfile.write("// This file is autogenerated by xous-core/loader/src/generate_fonts.py. Do not edit.\n")
        modfile.write("// The order of these modules impacts the link order, which changes the position in the binary image.\n")
        for k,v in fontdict.items():
            modfile.write("pub mod {};\n".format(k))

    with open('../../services/graphics-server/src/fontmap.rs', 'w') as mapfile:
        mapfile.write("// This file is autogenerated by xous-core/loader/src/generate_fonts.py. Do not edit.\n")
        mapfile.write("// This makes probably bad assumptions about how link order is computed. Be suspicious of these offsets.\n")
        mapfile.write("#![allow(dead_code)]\n")
        # Python iterators are deterministic, right......? so if I used the same iterator to make the link order it'll be the same here....right?
        offset = 0
        mapfile.write("pub const FONT_BASE: usize = 0x{:08x};\n".format(0x20530000))
        for k,v in fontdict.items():
            length = int(v) * 4
            mapfile.write("pub const {}_OFFSET: usize = 0x{:08x};\n".format(k.upper(), offset))
            mapfile.write("pub const {}_LEN: usize = 0x{:08x};\n".format(k.upper(), length))
            offset = offset + length
        mapfile.write("pub const FONT_TOTAL_LEN: usize = 0x{:08x};\n".format(offset))

if __name__ == "__main__":
    from datetime import datetime
    start = datetime.now()
    ret = main()
    print("Run completed in {}".format(datetime.now()-start))

    sys.exit(ret)
