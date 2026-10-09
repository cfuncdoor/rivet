#include <string>

#include "libhello.h"

int main() {
    if (greeting("World") != "Hello, World!") {
        return 1;
    }
    if (greeting("") != "Hello, !") {
        return 1;
    }
    return 0;
}
