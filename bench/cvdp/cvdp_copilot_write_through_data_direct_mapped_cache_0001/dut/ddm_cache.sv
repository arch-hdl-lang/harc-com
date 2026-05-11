module ddm_cache (
  input  logic        clk,           // Posedge clock
  input  logic        rst_n,         // Asynchronous Negedge reset
  input  logic [31:0] cpu_addr,      // Memory address emitted by the CPU
  input  logic [31:0] cpu_dout,      // Data emitted by CPU
  input  logic        cpu_strobe,    // CPU status signal to Cache to indicate it is going to perform a read or write operation
  input  logic        cpu_rw,        // cpu_rw == 1, Memory Write Operation ; cpu_rw == 0 , Memory Read Operation
  input  logic        uncached,      // uncached == 1 , IO port is accessed ; uncached == 0, Memory is accessed
  input  logic [31:0] mem_dout,      // Data emitted by memory
  input  logic        mem_ready,     // Memory is ready with the read data
  output logic [31:0] cpu_din,       // CPU Data coming from Memory through Cache
  output logic [31:0] mem_din,       // Memory Data coming from CPU through Cache
  output logic        cpu_ready,     // Cache is ready with data to be provided to CPU
  output logic        mem_strobe,    // Cache Status signal to Memory to indicate it is going to perform a read or write operation
  output logic        mem_rw,        // mem_rw == 1, Memory Write Operation ; mem_rw == 0 , Memory Read Operation
  output logic [31:0] mem_addr,      // Memory address to be accessed, emitted by the Cache
  output logic        cache_hit,     // Indicates a memory location is available in the cache
  output logic        cache_miss,    // Indicates a memory location is not available in the cache
  output logic [31:0] d_data_dout     // Data at a cache index
);

  logic         d_valid [0:63];
  logic [23:0]  d_tags  [0:63];
  logic [31:0]  d_data  [0:63];
  
  logic [23:0] tag;
  logic [5:0]  index;
  
  logic [31:0] cache_din;   
  logic        cache_write; 
  logic [23:0] d_tag_out;
  logic  valid;
  
  integer i;
  
  assign tag   = cpu_addr[31:8];
  assign index = cpu_addr[7:2]; 
  assign cache_write = ~uncached & (cpu_rw | cache_miss & mem_ready); 
  
  always_ff @(posedge clk or negedge rst_n) begin
    if (!rst_n) begin
        for (i = 0; i < 64; i = i + 1) begin
            d_valid[i] <= 0;
        end
    end else if (cache_write) begin
            d_valid[index] <= 1;
    end
  end

  assign cache_din = cpu_rw ? cpu_dout : mem_dout;
  always_ff@(posedge clk) begin
       if(cache_write) begin
         d_tags [index] <= tag;
         d_data [index] <= cache_din;
      end else begin
         d_tags [index] <= d_tags[index] + 32'd0;
         d_data [index] <= d_data[index] + 32'd0;
       end
  end  
  
  assign valid          = d_valid[index];
  assign d_tag_out      = d_tags [index];
  assign cache_dout     = d_data [index];
  assign cache_hit      = cpu_strobe & valid & (d_tag_out == tag);
  assign cache_miss     = cpu_strobe & (!valid | (d_tag_out != tag));
  assign mem_din        = cpu_dout;
  assign mem_addr       = cpu_addr;
  assign mem_rw         = cpu_rw;
  assign mem_strobe     = cpu_strobe | cache_miss;
  assign cpu_ready      = ~cpu_rw & cache_hit | (cache_miss | cpu_rw) & mem_ready;
  assign cpu_din        = cache_hit ? cache_dout : mem_dout;
  assign d_data_out     = cache_dout;
endmodule  
