#include <cstdio>
#include <cstring>

#include <zlib.h>

int main() {
    std::printf("zlib version: %s\n", zlibVersion());
    const char* input = "hello rivet";
    uLong src_len = static_cast<uLong>(std::strlen(input)) + 1;
    uLong coded_len = compressBound(src_len);
    Bytef* coded = new Bytef[coded_len];
    if (compress(coded, &coded_len, reinterpret_cast<const Bytef*>(input), src_len) != Z_OK) {
        std::puts("compress failed");
        delete[] coded;
        return 1;
    }
    char* back = new char[src_len];
    uLong back_len = src_len;
    int ok = uncompress(reinterpret_cast<Bytef*>(back), &back_len, coded, coded_len);
    delete[] coded;
    if (ok != Z_OK || std::strcmp(back, input) != 0) {
        std::puts("roundtrip failed");
        delete[] back;
        return 1;
    }
    std::printf("roundtrip ok: %s\n", back);
    delete[] back;
    return 0;
}
