
extern int eclipse_sigaltstack_record(const void* ss, void* old_ss,
                                      const void* caller);

int eclipse_sigaltstack(const void* ss, void* old_ss) {
  return eclipse_sigaltstack_record(ss, old_ss, __builtin_return_address(0));
}
