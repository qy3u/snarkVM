// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkVM library.

// The snarkVM library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkVM library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkVM library. If not, see <https://www.gnu.org/licenses/>.

use super::*;

impl<N: Network> FromBytes for ConfirmedTransaction<N> {
    /// Reads the confirmed transaction from a buffer.
    fn read_le<R: Read>(mut reader: R) -> IoResult<Self> {
        let variant = u8::read_le(&mut reader)?;
        match variant {
            0 => {
                // Read the index.
                let index = u32::read_le(&mut reader)?;
                // Read the transaction.
                let transaction = Transaction::<N>::read_le(&mut reader)?;
                // Read the number of finalize operations.
                let num_finalize = NumFinalizeSize::read_le(&mut reader)?;
                // Read the finalize operations.
                let finalize =
                    (0..num_finalize).map(|_| FromBytes::read_le(&mut reader)).collect::<Result<Vec<_>, _>>()?;
                // Return the confirmed transaction.
                Self::accepted_deploy(index, transaction, finalize).map_err(|e| error(e.to_string()))
            }
            1 => {
                // Read the index.
                let index = u32::read_le(&mut reader)?;
                // Read the transaction.
                let transaction = Transaction::<N>::read_le(&mut reader)?;
                // Read the number of finalize operations.
                let num_finalize = NumFinalizeSize::read_le(&mut reader)?;
                // Read the finalize operations.
                let finalize =
                    (0..num_finalize).map(|_| FromBytes::read_le(&mut reader)).collect::<Result<Vec<_>, _>>()?;
                // Return the confirmed transaction.
                Self::accepted_execute(index, transaction, finalize).map_err(|e| error(e.to_string()))
            }
            2 => {
                // Read the index.
                let index = u32::read_le(&mut reader)?;
                // Read the transaction.
                let transaction = Transaction::<N>::read_le(&mut reader)?;
                // Read the rejected deployment.
                let rejected = Deployment::<N>::read_le(&mut reader)?;
                // Return the confirmed transaction.
                Self::rejected_deploy(index, transaction, rejected).map_err(|e| error(e.to_string()))
            }
            3 => {
                // Read the index.
                let index = u32::read_le(&mut reader)?;
                // Read the transaction.
                let transaction = Transaction::<N>::read_le(&mut reader)?;
                // Read the rejected execution.
                let rejected = Execution::<N>::read_le(&mut reader)?;
                // Return the confirmed transaction.
                Self::rejected_execute(index, transaction, rejected).map_err(|e| error(e.to_string()))
            }
            4.. => Err(error(format!("Failed to decode confirmed transaction variant {variant}"))),
        }
    }
}

impl<N: Network> ToBytes for ConfirmedTransaction<N> {
    /// Writes the confirmed transaction to a buffer.
    fn write_le<W: Write>(&self, mut writer: W) -> IoResult<()> {
        match self {
            Self::AcceptedDeploy(index, transaction, finalize) => {
                // Write the variant.
                0u8.write_le(&mut writer)?;
                // Write the index.
                index.write_le(&mut writer)?;
                // Write the transaction.
                transaction.write_le(&mut writer)?;
                // Write the number of finalize operations.
                NumFinalizeSize::try_from(finalize.len()).map_err(|e| error(e.to_string()))?.write_le(&mut writer)?;
                // Write the finalize operations.
                finalize.iter().try_for_each(|finalize| finalize.write_le(&mut writer))
            }
            Self::AcceptedExecute(index, transaction, finalize) => {
                // Write the variant.
                1u8.write_le(&mut writer)?;
                // Write the index.
                index.write_le(&mut writer)?;
                // Write the transaction.
                transaction.write_le(&mut writer)?;
                // Write the number of finalize operations.
                NumFinalizeSize::try_from(finalize.len()).map_err(|e| error(e.to_string()))?.write_le(&mut writer)?;
                // Write the finalize operations.
                finalize.iter().try_for_each(|finalize| finalize.write_le(&mut writer))
            }
            Self::RejectedDeploy(index, transaction, rejected) => {
                // Write the variant.
                2u8.write_le(&mut writer)?;
                // Write the index.
                index.write_le(&mut writer)?;
                // Write the transaction.
                transaction.write_le(&mut writer)?;
                // Write the rejected deployment.
                rejected.write_le(&mut writer)
            }
            Self::RejectedExecute(index, transaction, rejected) => {
                // Write the variant.
                3u8.write_le(&mut writer)?;
                // Write the index.
                index.write_le(&mut writer)?;
                // Write the transaction.
                transaction.write_le(&mut writer)?;
                // Write the rejected execution.
                rejected.write_le(&mut writer)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::network::Testnet3;

    type CurrentNetwork = Testnet3;

    #[test]
    fn test_bytes() {
        for expected in crate::block::transactions::confirmed::test_helpers::sample_confirmed_transactions() {
            // Check the byte representation.
            let expected_bytes = expected.to_bytes_le().unwrap();
            assert_eq!(expected, ConfirmedTransaction::read_le(&expected_bytes[..]).unwrap());
            assert!(ConfirmedTransaction::<CurrentNetwork>::read_le(&expected_bytes[1..]).is_err());
        }
    }
}
