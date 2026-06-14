# Out-of-context Vivado synth + place (+ route) for the integrated ChannelizerCore,
# to get the real *combined* XC7Z010 (Pluto) LUT/FF/DSP48E1/BRAM utilization and
# timing (WNS) for the whole per-lane datapath, not just isolated blocks.
#   vivado -mode batch -source ooc_place.tcl -tclargs <verilog> <top> <period_ns>
set vfile  [lindex $argv 0]
set top    [lindex $argv 1]
set period [lindex $argv 2]

read_verilog $vfile
synth_design -top $top -part xc7z010clg225-1 -mode out_of_context -flatten_hierarchy full

# 62.5 MHz PL sync clock (16 ns) unless overridden
create_clock -name clk -period $period [get_ports clk]

opt_design
place_design
report_utilization -file ${top}_placed_util.rpt
phys_opt_design
route_design
report_utilization -file ${top}_routed_util.rpt
report_timing_summary -file ${top}_timing.rpt
puts "PLACE_DONE $top"
