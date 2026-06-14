# Out-of-context Vivado synthesis for a single Amaranth-generated module, to get
# real XC7Z010 (Pluto) LUT/FF/DSP48E1/BRAM utilization vs the Yosys estimates.
#   vivado -mode batch -source ooc_synth.tcl -tclargs <verilog> <top>
set vfile [lindex $argv 0]
set top   [lindex $argv 1]
read_verilog $vfile
synth_design -top $top -part xc7z010clg225-1 -mode out_of_context -flatten_hierarchy full
report_utilization -file ${top}_ooc_util.rpt
puts "OOC_DONE $top"
