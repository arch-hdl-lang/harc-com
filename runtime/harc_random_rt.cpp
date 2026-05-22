// harc_random_rt.cpp — Phase 5A runtime randomization scaffold.
//
// The current implementation is header-only because the solve entrypoints are
// templated and intentionally inert. This translation unit exists so build
// systems can link a stable runtime object before the backend grows real state.

#include "harc_random_rt.h"

namespace harc_rt {
namespace random {

static_assert(harc_seed_from(1, 2, 3) == harc_seed_from(1, 2, 3));
static_assert(harc_seed_from(1, 2, 3) != harc_seed_from(1, 2, 4));

} // namespace random
} // namespace harc_rt

