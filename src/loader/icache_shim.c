void eclipse_sync_icache(void* begin, void* end) {
  __builtin___clear_cache((char*)begin, (char*)end);
}
