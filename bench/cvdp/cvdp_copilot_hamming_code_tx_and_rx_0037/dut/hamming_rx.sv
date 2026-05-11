module hamming_rx 
#(
  parameter DATA_WIDTH   = 4,
  parameter PARITY_BIT   = 3,
  parameter ENCODED_DATA = PARITY_BIT + DATA_WIDTH + 1,
  parameter ENCODED_DATA_BIT = $clog2(ENCODED_DATA)
)
(
  input  [ENCODED_DATA-1:0] data_in, 
  output reg [DATA_WIDTH-1:0] data_out
);
  
  localparam PARITY = $clog2(PARITY_BIT);
  
  reg [PARITY_BIT-1:0] parity;
  reg [PARITY:0] j;
  reg [$clog2(DATA_WIDTH)-1:0] count;
  reg [ENCODED_DATA-1:0] corrected_data;

  reg [ENCODED_DATA_BIT:0] i,k;
  
  always @(*) 
  begin
    parity        = {PARITY_BIT{1'b0}};
    data_out      = {DATA_WIDTH{1'b0}};
    i             = {1'b1,{ENCODED_DATA_BIT{1'b1}}};
    j             = {1'b1,{PARITY{1'b1}}};
    k             = {1'b1,{ENCODED_DATA_BIT{1'b1}}};
    count         = 0;
    corrected_data = data_in;


    for (j = 0; j < PARITY_BIT; j = j + 1) 
    begin
      for (i = 1; i < ENCODED_DATA; i = i + 1) 
      begin
        if ((i & (1 << j)) != 0) 
        begin
          parity[j] = parity[j] ^ data_in[i];
        end
      end
    end
    
    corrected_data[parity] = (|parity) ? ~corrected_data[parity] : corrected_data[parity];

    for (k = 1; k < ENCODED_DATA; k = k + 1) 
    begin
      if ((k & (k - 1)) != 0) 
      begin 
        data_out[count] = corrected_data[k];
        count = count + 1;
      end
    end
  end
endmodule
