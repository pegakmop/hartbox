#!/usr/bin/env python3
"""Generate ios/rvemu.xcodeproj/project.pbxproj for the Hartbox target."""

import os

ROOT = os.path.dirname(os.path.abspath(__file__))  # ios/
APP = "RvemuApp"
PRODUCT = "Hartbox"
TEAM = "8SPG7WLCWL"
BUNDLE = "com.u6170252806.rvemu"

ids = {}
counter = 0


def oid(name):
    global counter
    if name not in ids:
        counter += 1
        ids[name] = "A1B2C3D4E5F6%011X" % counter
    return ids[name]


SOURCES = ["RvemuApp.swift", "ContentView.swift", "Terminal.swift", "TerminalView.swift", "Rvemu.swift", "Palette.swift", "Rssh.swift"]
RESOURCES = [("file", "vmlinuz-lts"), ("file", "alpine-minirootfs-3.24.2-riscv64.tar.gz"), ("catalog", "Assets.xcassets"), ("modules", "Modules")]
LIB = "librvemu_ios.a"
LIB_PATH = "rvemu-ios/target/aarch64-apple-ios/release/librvemu_ios.a"

out = []
out.append("// !$*UTF8*$!")
out.append("{")
out.append("\tarchiveVersion = 1;")
out.append("\tclasses = {};")
out.append("\tobjectVersion = 56;")
out.append("\tobjects = {")

# ---------------------------------------------------------------- PBXBuildFile
out.append("\n/* Begin PBXBuildFile section */")
for f in SOURCES:
    out.append(
        f"\t\t{oid('bf:' + f)} /* {f} in Sources */ = {{isa = PBXBuildFile; fileRef = {oid('fr:' + f)} /* {f} */; }};"
    )
for kind, name in RESOURCES:
    out.append(
        f"\t\t{oid('bf:' + name)} /* {name} in Resources */ = {{isa = PBXBuildFile; fileRef = {oid('fr:' + name)} /* {name} */; }};"
    )
out.append(
    f"\t\t{oid('bf:lib')} /* {LIB} in Frameworks */ = {{isa = PBXBuildFile; fileRef = {oid('fr:lib')} /* {LIB} */; }};"
)
out.append("/* End PBXBuildFile section */")

# ------------------------------------------------------------- PBXFileReference
out.append("\n/* Begin PBXFileReference section */")
for f in SOURCES:
    out.append(
        f"\t\t{oid('fr:' + f)} /* {f} */ = {{isa = PBXFileReference; lastKnownFileType = sourcecode.swift; path = {f}; sourceTree = \"<group>\"; }};"
    )
for kind, name in RESOURCES:
    if kind == "modules":
        out.append(
            f"\t\t{oid('fr:' + name)} /* {name} */ = {{isa = PBXFileReference; lastKnownFileType = folder; path = {name}; sourceTree = \"<group>\"; }};"
        )
    elif kind == "catalog":
        out.append(
            f"\t\t{oid('fr:' + name)} /* {name} */ = {{isa = PBXFileReference; lastKnownFileType = folder.assetcatalog; path = {name}; sourceTree = \"<group>\"; }};"
        )
    else:
        out.append(
            f"\t\t{oid('fr:' + name)} /* {name} */ = {{isa = PBXFileReference; lastKnownFileType = file; path = {name}; sourceTree = \"<group>\"; }};"
        )
out.append(
    f"\t\t{oid('fr:lib')} /* {LIB} */ = {{isa = PBXFileReference; lastKnownFileType = archive.ar; name = {LIB}; path = \"{LIB_PATH}\"; sourceTree = SOURCE_ROOT; }};"
)
out.append(
    f"\t\t{oid('fr:product')} /* {PRODUCT}.app */ = {{isa = PBXFileReference; explicitFileType = wrapper.application; includeInIndex = 0; path = {PRODUCT}.app; sourceTree = BUILT_PRODUCTS_DIR; }};"
)
out.append("/* End PBXFileReference section */")

# ----------------------------------------------------------- PBXFrameworksBuildPhase
out.append("\n/* Begin PBXFrameworksBuildPhase section */")
out.append(f"\t\t{oid('phase:frameworks')} /* Frameworks */ = {{")
out.append("\t\t\tisa = PBXFrameworksBuildPhase;")
out.append("\t\t\tbuildActionMask = 2147483647;")
out.append("\t\t\tfiles = (")
out.append(f"\t\t\t\t{oid('bf:lib')} /* {LIB} in Frameworks */,")
out.append("\t\t\t);")
out.append("\t\t\trunOnlyForDeploymentPostprocessing = 0;")
out.append("\t\t};")
out.append("/* End PBXFrameworksBuildPhase section */")

# -------------------------------------------------------------------- PBXGroup
out.append("\n/* Begin PBXGroup section */")
out.append(f"\t\t{oid('group:main')} = {{")
out.append("\t\t\tisa = PBXGroup;")
out.append("\t\t\tchildren = (")
out.append(f"\t\t\t\t{oid('group:app')} /* RvemuApp */,")
out.append(f"\t\t\t\t{oid('group:resources')} /* Resources */,")
out.append(f"\t\t\t\t{oid('group:frameworks')} /* Frameworks */,")
out.append(f"\t\t\t\t{oid('group:products')} /* Products */,")
out.append("\t\t\t);")
out.append("\t\t\tsourceTree = \"<group>\";")
out.append("\t\t};")

out.append(f"\t\t{oid('group:app')} /* RvemuApp */ = {{")
out.append("\t\t\tisa = PBXGroup;")
out.append("\t\t\tchildren = (")
for f in SOURCES:
    out.append(f"\t\t\t\t{oid('fr:' + f)} /* {f} */,")
out.append(f"\t\t\t\t{oid('fr:Assets.xcassets')} /* Assets.xcassets */,")
out.append(f"\t\t\t\t{oid('fr:Modules')} /* Modules */,")
out.append("\t\t\t);")
out.append(f"\t\t\tpath = {APP};")
out.append("\t\t\tsourceTree = \"<group>\";")
out.append("\t\t};")

out.append(f"\t\t{oid('group:resources')} /* Resources */ = {{")
out.append("\t\t\tisa = PBXGroup;")
out.append("\t\t\tchildren = (")
for kind, name in RESOURCES:
    if kind == "file":
        out.append(f"\t\t\t\t{oid('fr:' + name)} /* {name} */,")
out.append("\t\t\t);")
out.append("\t\t\tname = Resources;")
out.append(f"\t\t\tpath = {APP}/Resources;")
out.append("\t\t\tsourceTree = \"<group>\";")
out.append("\t\t};")

out.append(f"\t\t{oid('group:frameworks')} /* Frameworks */ = {{")
out.append("\t\t\tisa = PBXGroup;")
out.append("\t\t\tchildren = (")
out.append(f"\t\t\t\t{oid('fr:lib')} /* {LIB} */,")
out.append("\t\t\t);")
out.append("\t\t\tname = Frameworks;")
out.append("\t\t\tsourceTree = \"<group>\";")
out.append("\t\t};")

out.append(f"\t\t{oid('group:products')} /* Products */ = {{")
out.append("\t\t\tisa = PBXGroup;")
out.append("\t\t\tchildren = (")
out.append(f"\t\t\t\t{oid('fr:product')} /* {PRODUCT}.app */,")
out.append("\t\t\t);")
out.append("\t\t\tname = Products;")
out.append("\t\t\tsourceTree = \"<group>\";")
out.append("\t\t};")
out.append("/* End PBXGroup section */")

# ------------------------------------------------------------- PBXNativeTarget
out.append("\n/* Begin PBXNativeTarget section */")
out.append(f"\t\t{oid('target')} /* {PRODUCT} */ = {{")
out.append("\t\t\tisa = PBXNativeTarget;")
out.append(f"\t\t\tbuildConfigurationList = {oid('cfglist:target')} /* Build configuration list for PBXNativeTarget \"{PRODUCT}\" */;")
out.append("\t\t\tbuildPhases = (")
out.append(f"\t\t\t\t{oid('phase:script')} /* Build rust library */,")
out.append(f"\t\t\t\t{oid('phase:sources')} /* Sources */,")
out.append(f"\t\t\t\t{oid('phase:frameworks')} /* Frameworks */,")
out.append(f"\t\t\t\t{oid('phase:resources')} /* Resources */,")
out.append("\t\t\t);")
out.append("\t\t\tbuildRules = ();")
out.append("\t\t\tdependencies = ();")
out.append(f"\t\t\tname = {PRODUCT};")
out.append(f"\t\t\tproductName = {PRODUCT};")
out.append(f"\t\t\tproductReference = {oid('fr:product')} /* {PRODUCT}.app */;")
out.append("\t\t\tproductType = \"com.apple.product-type.application\";")
out.append("\t\t};")
out.append("/* End PBXNativeTarget section */")

# ------------------------------------------------------------------- PBXProject
out.append("\n/* Begin PBXProject section */")
out.append(f"\t\t{oid('project')} /* Project object */ = {{")
out.append("\t\t\tisa = PBXProject;")
out.append("\t\t\tattributes = {")
out.append("\t\t\t\tBuildIndependentTargetsInParallel = 1;")
out.append("\t\t\t\tLastSwiftUpdateCheck = 1600;")
out.append("\t\t\t\tLastUpgradeCheck = 1600;")
out.append("\t\t\t\tTargetAttributes = {")
out.append(f"\t\t\t\t\t{oid('target')} = {{")
out.append("\t\t\t\t\t\tCreatedOnToolsVersion = 16.0;")
out.append("\t\t\t\t\t};")
out.append("\t\t\t\t};")
out.append("\t\t\t};")
out.append(f"\t\t\tbuildConfigurationList = {oid('cfglist:project')} /* Build configuration list for PBXProject \"rvemu\" */;")
out.append("\t\t\tcompatibilityVersion = \"Xcode 14.0\";")
out.append("\t\t\tdevelopmentRegion = en;")
out.append("\t\t\thasScannedForEncodings = 0;")
out.append("\t\t\tknownRegions = (en, Base);")
out.append(f"\t\t\tmainGroup = {oid('group:main')};")
out.append(f"\t\t\tproductRefGroup = {oid('group:products')} /* Products */;")
out.append("\t\t\tprojectDirPath = \"\";")
out.append("\t\t\tprojectRoot = \"\";")
out.append("\t\t\ttargets = (")
out.append(f"\t\t\t\t{oid('target')} /* {PRODUCT} */,")
out.append("\t\t\t);")
out.append("\t\t};")
out.append("/* End PBXProject section */")

# -------------------------------------------------------------- PBXResourcesBuildPhase
out.append("\n/* Begin PBXResourcesBuildPhase section */")
out.append(f"\t\t{oid('phase:resources')} /* Resources */ = {{")
out.append("\t\t\tisa = PBXResourcesBuildPhase;")
out.append("\t\t\tbuildActionMask = 2147483647;")
out.append("\t\t\tfiles = (")
for kind, name in RESOURCES:
    if kind in ("file", "modules"):
        out.append(f"\t\t\t\t{oid('bf:' + name)} /* {name} in Resources */,")
out.append(f"\t\t\t\t{oid('bf:Assets.xcassets')} /* Assets.xcassets in Resources */,")
out.append("\t\t\t);")
out.append("\t\t\trunOnlyForDeploymentPostprocessing = 0;")
out.append("\t\t};")
out.append("/* End PBXResourcesBuildPhase section */")

# ---------------------------------------------------------------- PBXShellScriptBuildPhase
out.append("\n/* Begin PBXShellScriptBuildPhase section */")
out.append(f"\t\t{oid('phase:script')} /* Build rust library */ = {{")
out.append("\t\t\tisa = PBXShellScriptBuildPhase;")
out.append("\t\t\talwaysOutOfDate = 1;")
out.append("\t\t\tbuildActionMask = 2147483647;")
out.append("\t\t\tfiles = ();")
out.append("\t\t\tinputFileListPaths = ();")
out.append("\t\t\tinputPaths = ();")
out.append("\t\t\tname = \"Build rust library\";")
out.append("\t\t\toutputFileListPaths = ();")
out.append("\t\t\toutputPaths = ();")
out.append("\t\t\trunOnlyForDeploymentPostprocessing = 0;")
out.append("\t\t\tshellPath = /bin/sh;")
out.append("\t\t\tshellScript = \"export PATH=\\\"$HOME/.cargo/bin:$PATH\\\"\\ncd \\\"$SRCROOT/rvemu-ios\\\"\\ncargo build --release --target aarch64-apple-ios 2>&1\\n\";")
out.append("\t\t};")
out.append("/* End PBXShellScriptBuildPhase section */")

# --------------------------------------------------------------------- PBXSourcesBuildPhase
out.append("\n/* Begin PBXSourcesBuildPhase section */")
out.append(f"\t\t{oid('phase:sources')} /* Sources */ = {{")
out.append("\t\t\tisa = PBXSourcesBuildPhase;")
out.append("\t\t\tbuildActionMask = 2147483647;")
out.append("\t\t\tfiles = (")
for f in SOURCES:
    out.append(f"\t\t\t\t{oid('bf:' + f)} /* {f} in Sources */,")
out.append("\t\t\t);")
out.append("\t\t\trunOnlyForDeploymentPostprocessing = 0;")
out.append("\t\t};")
out.append("/* End PBXSourcesBuildPhase section */")

# --------------------------------------------------------------- XCBuildConfiguration
out.append("\n/* Begin XCBuildConfiguration section */")
project_common = [
    "ALWAYS_SEARCH_USER_PATHS = NO",
    "CLANG_ANALYZER_NONNULL = YES",
    "CLANG_ENABLE_MODULES = YES",
    "CLANG_ENABLE_OBJC_ARC = YES",
    "COPY_PHASE_STRIP = NO",
    "ENABLE_STRICT_OBJC_MSGSEND = YES",
    "ENABLE_USER_SCRIPT_SANDBOXING = NO",
    "GCC_C_LANGUAGE_STANDARD = gnu17",
    "GCC_NO_COMMON_BLOCKS = YES",
    "IPHONEOS_DEPLOYMENT_TARGET = 16.0",
    "SDKROOT = iphoneos",
    "SWIFT_VERSION = 5.0",
    "TARGETED_DEVICE_FAMILY = 1",
]
out.append(f"\t\t{oid('cfg:proj:release')} /* Release */ = {{")
out.append("\t\t\tisa = XCBuildConfiguration;")
out.append("\t\t\tbuildSettings = {")
for line in project_common:
    out.append(f"\t\t\t\t{line};")
out.append("\t\t\t\tENABLE_NS_ASSERTIONS = NO;")
out.append("\t\t\t\tSWIFT_COMPILATION_MODE = wholemodule;")
out.append("\t\t\t\tVALIDATE_PRODUCT = YES;")
out.append("\t\t\t};")
out.append("\t\t\tname = Release;")
out.append("\t\t};")
out.append(f"\t\t{oid('cfg:proj:debug')} /* Debug */ = {{")
out.append("\t\t\tisa = XCBuildConfiguration;")
out.append("\t\t\tbuildSettings = {")
for line in project_common:
    out.append(f"\t\t\t\t{line};")
out.append("\t\t\t\tDEBUG_INFORMATION_FORMAT = dwarf;")
out.append("\t\t\t\tENABLE_TESTABILITY = YES;")
out.append("\t\t\t\tGCC_OPTIMIZATION_LEVEL = 0;")
out.append("\t\t\t\tONLY_ACTIVE_ARCH = YES;")
out.append("\t\t\t\tSWIFT_ACTIVE_COMPILATION_CONDITIONS = DEBUG;")
out.append("\t\t\t\tSWIFT_OPTIMIZATION_LEVEL = \"-Onone\";")
out.append("\t\t\t};")
out.append("\t\t\tname = Debug;")
out.append("\t\t};")

target_common = [
    "ASSETCATALOG_COMPILER_APPICON_NAME = AppIcon",
    "CODE_SIGN_STYLE = Automatic",
    f"DEVELOPMENT_TEAM = {TEAM}",
    "GENERATE_INFOPLIST_FILE = NO",
    f"INFOPLIST_FILE = {APP}/Info.plist",
    f"HEADER_SEARCH_PATHS = \"$(SRCROOT)/rvemu-ios/include\"",
    f"LIBRARY_SEARCH_PATHS = \"$(SRCROOT)/rvemu-ios/target/aarch64-apple-ios/release\"",
    'OTHER_LDFLAGS = "$(inherited) -lresolv"',
    "LD_RUNPATH_SEARCH_PATHS = \"$(inherited) @executable_path/Frameworks\"",
    "MARKETING_VERSION = 1.1.0",
    "CURRENT_PROJECT_VERSION = 2",
    f"PRODUCT_BUNDLE_IDENTIFIER = {BUNDLE}",
    f"PRODUCT_NAME = {PRODUCT}",
    f"SWIFT_OBJC_BRIDGING_HEADER = {APP}/Rvemu-Bridging-Header.h",
    "SWIFT_EMIT_LOC_STRINGS = NO",
]
out.append(f"\t\t{oid('cfg:tgt:release')} /* Release */ = {{")
out.append("\t\t\tisa = XCBuildConfiguration;")
out.append("\t\t\tbuildSettings = {")
for line in target_common:
    out.append(f"\t\t\t\t{line};")
out.append("\t\t\t};")
out.append("\t\t\tname = Release;")
out.append("\t\t};")
out.append(f"\t\t{oid('cfg:tgt:debug')} /* Debug */ = {{")
out.append("\t\t\tisa = XCBuildConfiguration;")
out.append("\t\t\tbuildSettings = {")
for line in target_common:
    out.append(f"\t\t\t\t{line};")
out.append("\t\t\t};")
out.append("\t\t\tname = Debug;")
out.append("\t\t};")
out.append("/* End XCBuildConfiguration section */")

# ------------------------------------------------------------- XCConfigurationList
out.append("\n/* Begin XCConfigurationList section */")
out.append(f"\t\t{oid('cfglist:project')} /* Build configuration list for PBXProject \"rvemu\" */ = {{")
out.append("\t\t\tisa = XCConfigurationList;")
out.append("\t\t\tbuildConfigurations = (")
out.append(f"\t\t\t\t{oid('cfg:proj:debug')} /* Debug */,")
out.append(f"\t\t\t\t{oid('cfg:proj:release')} /* Release */,")
out.append("\t\t\t);")
out.append("\t\t\tdefaultConfigurationIsVisible = 0;")
out.append("\t\t\tdefaultConfigurationName = Release;")
out.append("\t\t};")
out.append(f"\t\t{oid('cfglist:target')} /* Build configuration list for PBXNativeTarget \"{PRODUCT}\" */ = {{")
out.append("\t\t\tisa = XCConfigurationList;")
out.append("\t\t\tbuildConfigurations = (")
out.append(f"\t\t\t\t{oid('cfg:tgt:debug')} /* Debug */,")
out.append(f"\t\t\t\t{oid('cfg:tgt:release')} /* Release */,")
out.append("\t\t\t);")
out.append("\t\t\tdefaultConfigurationIsVisible = 0;")
out.append("\t\t\tdefaultConfigurationName = Release;")
out.append("\t\t};")
out.append("/* End XCConfigurationList section */")

out.append("\t};")
out.append(f"\trootObject = {oid('project')} /* Project object */;")
out.append("}")

proj_dir = os.path.join(ROOT, "rvemu.xcodeproj")
os.makedirs(proj_dir, exist_ok=True)
with open(os.path.join(proj_dir, "project.pbxproj"), "w") as f:
    f.write("\n".join(out) + "\n")
print("wrote", os.path.join(proj_dir, "project.pbxproj"), len(out), "lines")
