// A downstream project consuming tf_tree through find_package(tf_tree CONFIG).
// It gets the headers and cxx_std_17 from the imported target alone — no
// include paths, no -lpthread, nothing.
#include <tf_tree.hpp>
#include <cstdio>

int main() {
    if (tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR) != TFT_OK) return 1;
    if (tft_layout_size(TFT_LAYOUT_MAT4_COL) != 128) return 2;
    // C++17 must have come from INTERFACE_COMPILE_FEATURES, not from a flag we set.
    static_assert(__cplusplus >= 201703L, "cxx_std_17 was not propagated by the package");
    std::printf("consumer ok: abi %u.%u\n", tft_abi_version_major(), tft_abi_version_minor());
    return 0;
}
