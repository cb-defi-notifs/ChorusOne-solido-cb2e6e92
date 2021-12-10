import * as BufferLayout from '@solana/buffer-layout';
import { TOKEN_PROGRAM_ID } from '@solana/spl-token';
import {
  PublicKey,
  SystemProgram,
  TransactionInstruction,
} from '@solana/web3.js';
import { Lamports, ProgramAddresses } from '../types';
import { findAuthorityProgramAddress } from '../utils';

export const getDepositInstruction = async (
  senderAddress: PublicKey,
  recipientStSolAddress: PublicKey,
  programAddresses: ProgramAddresses,
  amount: Lamports
) => {
  const dataLayout = BufferLayout.struct([
    BufferLayout.u8('instruction'),
    BufferLayout.nu64('amount'),
  ]);

  const data = Buffer.alloc(dataLayout.span);
  dataLayout.encode({ instruction: 1, amount: amount.lamports }, data);

  const reserveAccountAddress = await findAuthorityProgramAddress(
    programAddresses,
    'reserve_account'
  );

  const mintAuthorityAddress = await findAuthorityProgramAddress(
    programAddresses,
    'mint_authority'
  );

  const keys = [
    {
      pubkey: programAddresses.solidoInstanceId,
      isSigner: false,
      isWritable: true,
    },
    { pubkey: senderAddress, isSigner: true, isWritable: true },
    {
      pubkey: recipientStSolAddress,
      isSigner: false,
      isWritable: true,
    },
    {
      pubkey: programAddresses.stSolMintAddress,
      isSigner: false,
      isWritable: true,
    },
    { pubkey: reserveAccountAddress, isSigner: false, isWritable: true },
    { pubkey: mintAuthorityAddress, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
  ];

  return new TransactionInstruction({
    keys,
    data,
    programId: programAddresses.solidoProgramId,
  });
};
