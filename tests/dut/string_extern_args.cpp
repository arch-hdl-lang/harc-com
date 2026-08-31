#include <cstdint>
#include <cstring>

extern "C" uint64_t ref_string_choice(
    const char* key,
    const char* choices,
    uint64_t default_index,
    uint64_t required) {
  if (std::strcmp(key, "policy") != 0 ||
      std::strcmp(choices, "zero,one,two") != 0 || default_index != 0 ||
      required) {
    return 99;
  }
  return 1;
}

extern "C" uint64_t ref_string_bytes(const char* value) {
  static constexpr char expected[] =
      "quote:\" slash:\\ newline:\n carriage:\r tab:\t nul:\000Z literal:\\n "
      "unknown:\\q";
  return std::memcmp(value, expected, sizeof(expected)) == 0 ? 1 : 0;
}
