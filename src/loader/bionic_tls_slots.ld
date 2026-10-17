SECTIONS
{
  .tdata :
  {
    KEEP(*(.tdata.eclipse_bionic_slots))
    *(.tdata .tdata.* .gnu.linkonce.td.*)
  }
}
INSERT BEFORE .tbss;
