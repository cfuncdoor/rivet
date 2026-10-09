#include <iostream>

#include "libhello.h"

int main() {
    std::cout << greeting("lib") << " answer=" << answer() << "\n";
    return 0;
}
