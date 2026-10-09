// buildscript-demo main: consumes what build.rivet.cpp provides —
// the `generated` include dir (for "build_info.h") and HAVE_BUILD_INFO.
#include <iostream>

#ifdef HAVE_BUILD_INFO
#include "build_info.h"
#endif

int main() {
#ifdef HAVE_BUILD_INFO
  std::cout << "buildscript-demo: " << BUILD_INFO_VERSION << "\n";
#else
  std::cout << "buildscript-demo: built without build-script info\n";
#endif
  return 0;
}
