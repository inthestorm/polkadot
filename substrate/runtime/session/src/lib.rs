// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Substrate Demo.

// Substrate Demo is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate Demo is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate Demo.  If not, see <http://www.gnu.org/licenses/>.

//! Session manager: is told the validators and allows them to manage their session keys for the
//! consensus module.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
extern crate serde;

#[cfg(feature = "std")]
#[macro_use]
extern crate serde_derive;

#[cfg(any(feature = "std", test))]
extern crate substrate_keyring as keyring;

#[cfg(any(feature = "std", test))]
extern crate substrate_primitives;

#[cfg_attr(feature = "std", macro_use)]
extern crate substrate_runtime_std as rstd;

#[macro_use]
extern crate substrate_runtime_support as runtime_support;

extern crate substrate_runtime_io as runtime_io;
extern crate substrate_codec as codec;
extern crate substrate_runtime_primitives as primitives;
extern crate substrate_runtime_consensus as consensus;
extern crate substrate_runtime_system as system;
extern crate substrate_runtime_timestamp as timestamp;

use rstd::prelude::*;
use primitives::traits::{Zero, One, RefInto, Executable, Convert, As};
use runtime_support::{StorageValue, StorageMap};
use runtime_support::dispatch::Result;

/// A session has changed.
pub trait OnSessionChange<T> {
	/// Session has changed.
	fn on_session_change(normal_rotation: bool, time_elapsed: T);
}

impl<T> OnSessionChange<T> for () {
	fn on_session_change(_: bool, _: T) {}
}

pub trait Trait: timestamp::Trait {
	type ConvertAccountIdToSessionKey: Convert<Self::AccountId, Self::SessionKey>;
	type OnSessionChange: OnSessionChange<Self::Moment>;
}

decl_module! {
	pub struct Module<T: Trait>;

	#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
	pub enum Call where aux: T::PublicAux {
		fn set_key(aux, key: T::SessionKey) -> Result = 0;
	}

	#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
	pub enum PrivCall {
		fn set_length(new: T::BlockNumber) -> Result = 0;
		fn force_new_session(normal_rotation: bool) -> Result = 1;
	}
}

decl_storage! {
	trait Store for Module<T: Trait>;

	// The current set of validators.
	pub Validators get(validators): b"ses:val" => required Vec<T::AccountId>;
	// Current length of the session.
	pub SessionLength get(length): b"ses:len" => required T::BlockNumber;
	// Current index of the session.
	pub CurrentIndex get(current_index): b"ses:ind" => required T::BlockNumber;
	// Timestamp when current session started.
	pub CurrentStart get(current_start): b"ses:current_start" => required T::Moment;
	// Percent by which the session must necessarily finish late before we early-exit the session.
	pub BrokenPercentLate get(broken_percent_late): b"ses:broken_percent_late" => required T::Moment;

	// New session is being forced is this entry exists; in which case, the boolean value is whether
	// the new session should be considered a normal rotation (rewardable) or exceptional (slashable).
	pub ForcingNewSession get(forcing_new_session): b"ses:forcing_new_session" => bool;
	// Block at which the session length last changed.
	LastLengthChange: b"ses:llc" => T::BlockNumber;
	// The next key for a given validator.
	NextKeyFor: b"ses:nxt:" => map [ T::AccountId => T::SessionKey ];
	// The next session length.
	NextSessionLength: b"ses:nln" => T::BlockNumber;
}

impl<T: Trait> Module<T> {
	/// The number of validators currently.
	pub fn validator_count() -> u32 {
		<Validators<T>>::get().len() as u32	// TODO: can probably optimised
	}

	/// The last length change, if there was one, zero if not.
	pub fn last_length_change() -> T::BlockNumber {
		<LastLengthChange<T>>::get().unwrap_or_else(T::BlockNumber::zero)
	}

	/// Sets the session key of `_validator` to `_key`. This doesn't take effect until the next
	/// session.
	fn set_key(aux: &T::PublicAux, key: T::SessionKey) -> Result {
		// set new value for next session
		<NextKeyFor<T>>::insert(aux.ref_into(), key);
		Ok(())
	}

	/// Set a new era length. Won't kick in until the next era change (at current length).
	fn set_length(new: T::BlockNumber) -> Result {
		<NextSessionLength<T>>::put(new);
		Ok(())
	}

	/// Forces a new session.
	pub fn force_new_session(normal_rotation: bool) -> Result {
		<ForcingNewSession<T>>::put(normal_rotation);
		Ok(())
	}

	// INTERNAL API (available to other runtime modules)

	/// Set the current set of validators.
	///
	/// Called by `staking::next_era()` only. `next_session` should be called after this in order to
	/// update the session keys to the next validator set.
	pub fn set_validators(new: &[T::AccountId]) {
		<Validators<T>>::put(&new.to_vec());			// TODO: optimise.
		<consensus::Module<T>>::set_authorities(
			&new.iter().cloned().map(T::ConvertAccountIdToSessionKey::convert).collect::<Vec<_>>()
		);
	}

	/// Hook to be called after transaction processing.
	pub fn check_rotate_session() {
		// do this last, after the staking system has had chance to switch out the authorities for the
		// new set.
		// check block number and call next_session if necessary.
		let block_number = <system::Module<T>>::block_number();
		let is_final_block = ((block_number - Self::last_length_change()) % Self::length()).is_zero();
		let broken_validation = Self::broken_validation();
		if let Some(normal_rotation) = Self::forcing_new_session() {
			Self::rotate_session(normal_rotation, is_final_block);
			<ForcingNewSession<T>>::kill();
		} else if is_final_block || broken_validation {
			Self::rotate_session(!broken_validation, is_final_block);
		}
	}

	/// Move onto next session: register the new authority set.
	pub fn rotate_session(normal_rotation: bool, is_final_block: bool) {
		let now = <timestamp::Module<T>>::get();
		let time_elapsed = now.clone() - Self::current_start();

		// Increment current session index.
		<CurrentIndex<T>>::put(<CurrentIndex<T>>::get() + One::one());
		<CurrentStart<T>>::put(now);

		// Enact era length change.
		let len_changed = if let Some(next_len) = <NextSessionLength<T>>::take() {
			<SessionLength<T>>::put(next_len);
			true
		} else {
			false
		};
		if len_changed || !is_final_block {
			let block_number = <system::Module<T>>::block_number();
			<LastLengthChange<T>>::put(block_number);
		}

		T::OnSessionChange::on_session_change(normal_rotation, time_elapsed);

		// Update any changes in session keys.
		Self::validators().iter().enumerate().for_each(|(i, v)| {
			if let Some(n) = <NextKeyFor<T>>::take(v) {
				<consensus::Module<T>>::set_authority(i as u32, &n);
			}
		});
	}

	/// Get the time that should have elapsed over a session if everything was working perfectly.
	pub fn ideal_session_duration() -> T::Moment {
		let block_period = <timestamp::Module<T>>::block_period();
		let session_length = <T::Moment as As<T::BlockNumber>>::sa(Self::length());
		session_length * block_period
	}

	/// Number of blocks remaining in this session, not counting this one. If the session is
	/// due to rotate at the end of this block, then it will return 0. If the just began, then
	/// it will return `Self::length() - 1`.
	pub fn blocks_remaining() -> T::BlockNumber {
		let length = Self::length();
		let length_minus_1 = length - One::one();
		let block_number = <system::Module<T>>::block_number();
		length_minus_1 - (block_number - Self::last_length_change() + length_minus_1) % length
	}

	/// Returns `true` if the current validator set is taking took long to validate blocks.
	pub fn broken_validation() -> bool {
		let now = <timestamp::Module<T>>::get();
		let block_period = <timestamp::Module<T>>::block_period();
		let blocks_remaining = Self::blocks_remaining();
		let blocks_remaining = <T::Moment as As<T::BlockNumber>>::sa(blocks_remaining);
		now + blocks_remaining * block_period >
			Self::current_start() + Self::ideal_session_duration() *
				(T::Moment::sa(100) + Self::broken_percent_late()) / T::Moment::sa(100)
	}
}

impl<T: Trait> Executable for Module<T> {
	fn execute() {
		Self::check_rotate_session();
	}
}

#[cfg(any(feature = "std", test))]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct GenesisConfig<T: Trait> {
	pub session_length: T::BlockNumber,
	pub validators: Vec<T::AccountId>,
	pub broken_percent_late: T::Moment,
}

#[cfg(any(feature = "std", test))]
impl<T: Trait> Default for GenesisConfig<T> {
	fn default() -> Self {
		use primitives::traits::As;
		GenesisConfig {
			session_length: T::BlockNumber::sa(1000),
			validators: vec![],
			broken_percent_late: T::Moment::sa(30),
		}
	}
}

#[cfg(any(feature = "std", test))]
impl<T: Trait> primitives::BuildStorage for GenesisConfig<T>
{
	fn build_storage(self) -> ::std::result::Result<runtime_io::TestExternalities, String> {
		use codec::Encode;
		use primitives::traits::As;
		Ok(map![
			Self::hash(<SessionLength<T>>::key()).to_vec() => self.session_length.encode(),
			Self::hash(<CurrentIndex<T>>::key()).to_vec() => T::BlockNumber::sa(0).encode(),
			Self::hash(<CurrentStart<T>>::key()).to_vec() => T::Moment::zero().encode(),
			Self::hash(<Validators<T>>::key()).to_vec() => self.validators.encode(),
			Self::hash(<BrokenPercentLate<T>>::key()).to_vec() => self.broken_percent_late.encode()
		])
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use runtime_io::with_externalities;
	use substrate_primitives::H256;
	use primitives::BuildStorage;
	use primitives::traits::{HasPublicAux, Identity, BlakeTwo256};
	use primitives::testing::{Digest, Header};

	#[derive(Clone, Eq, PartialEq)]
	pub struct Test;
	impl HasPublicAux for Test {
		type PublicAux = u64;
	}
	impl consensus::Trait for Test {
		type PublicAux = <Self as HasPublicAux>::PublicAux;
		type SessionKey = u64;
	}
	impl system::Trait for Test {
		type Index = u64;
		type BlockNumber = u64;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type Digest = Digest;
		type AccountId = u64;
		type Header = Header;
	}
	impl timestamp::Trait for Test {
		const TIMESTAMP_SET_POSITION: u32 = 0;
		type Moment = u64;
	}
	impl Trait for Test {
		type ConvertAccountIdToSessionKey = Identity;
		type OnSessionChange = ();
	}

	type System = system::Module<Test>;
	type Consensus = consensus::Module<Test>;
	type Timestamp = timestamp::Module<Test>;
	type Session = Module<Test>;

	fn new_test_ext() -> runtime_io::TestExternalities {
		let mut t = system::GenesisConfig::<Test>::default().build_storage().unwrap();
		t.extend(consensus::GenesisConfig::<Test>{
			code: vec![],
			authorities: vec![1, 2, 3],
		}.build_storage().unwrap());
		t.extend(timestamp::GenesisConfig::<Test>{
			period: 5,
		}.build_storage().unwrap());
		t.extend(GenesisConfig::<Test>{
			session_length: 2,
			validators: vec![1, 2, 3],
			broken_percent_late: 30,
		}.build_storage().unwrap());
		t
	}

	#[test]
	fn simple_setup_should_work() {
		with_externalities(&mut new_test_ext(), || {
			assert_eq!(Consensus::authorities(), vec![1, 2, 3]);
			assert_eq!(Session::length(), 2);
			assert_eq!(Session::validators(), vec![1, 2, 3]);
		});
	}

	#[test]
	fn should_identify_broken_validation() {
		with_externalities(&mut new_test_ext(), || {
			System::set_block_number(2);
			assert_eq!(Session::blocks_remaining(), 0);
			Timestamp::set_timestamp(0);
			assert_ok!(Session::set_length(3));
			Session::check_rotate_session();
			assert_eq!(Session::current_index(), 1);
			assert_eq!(Session::length(), 3);
			assert_eq!(Session::current_start(), 0);
			assert_eq!(Session::ideal_session_duration(), 15);
			// ideal end = 0 + 15 * 3 = 15
			// broken_limit = 15 * 130 / 100 = 19
		
			System::set_block_number(3);
			assert_eq!(Session::blocks_remaining(), 2);
			Timestamp::set_timestamp(9);				// earliest end = 9 + 2 * 5 = 19; OK.
			assert!(!Session::broken_validation());
			Session::check_rotate_session();

			System::set_block_number(4);
			assert_eq!(Session::blocks_remaining(), 1);
			Timestamp::set_timestamp(15);				// another 1 second late. earliest end = 15 + 1 * 5 = 20; broken.
			assert!(Session::broken_validation());
			Session::check_rotate_session();
			assert_eq!(Session::current_index(), 2);
		});
	}

	#[test]
	fn should_work_with_early_exit() {
		with_externalities(&mut new_test_ext(), || {
			System::set_block_number(1);
			assert_ok!(Session::set_length(10));
			assert_eq!(Session::blocks_remaining(), 1);
			Session::check_rotate_session();

			System::set_block_number(2);
			assert_eq!(Session::blocks_remaining(), 0);
			Session::check_rotate_session();
			assert_eq!(Session::length(), 10);
		
			System::set_block_number(7);
			assert_eq!(Session::current_index(), 1);
			assert_eq!(Session::blocks_remaining(), 5);
			assert_ok!(Session::force_new_session(false));
			Session::check_rotate_session();

			System::set_block_number(8);
			assert_eq!(Session::current_index(), 2);
			assert_eq!(Session::blocks_remaining(), 9);
			Session::check_rotate_session();

			System::set_block_number(17);
			assert_eq!(Session::current_index(), 2);
			assert_eq!(Session::blocks_remaining(), 0);
			Session::check_rotate_session();

			System::set_block_number(18);
			assert_eq!(Session::current_index(), 3);
		});
	}

	#[test]
	fn session_length_change_should_work() {
		with_externalities(&mut new_test_ext(), || {
			// Block 1: Change to length 3; no visible change.
			System::set_block_number(1);
			assert_ok!(Session::set_length(3));
			Session::check_rotate_session();
			assert_eq!(Session::length(), 2);
			assert_eq!(Session::current_index(), 0);

			// Block 2: Length now changed to 3. Index incremented.
			System::set_block_number(2);
			assert_ok!(Session::set_length(3));
			Session::check_rotate_session();
			assert_eq!(Session::length(), 3);
			assert_eq!(Session::current_index(), 1);

			// Block 3: Length now changed to 3. Index incremented.
			System::set_block_number(3);
			Session::check_rotate_session();
			assert_eq!(Session::length(), 3);
			assert_eq!(Session::current_index(), 1);

			// Block 4: Change to length 2; no visible change.
			System::set_block_number(4);
			assert_ok!(Session::set_length(2));
			Session::check_rotate_session();
			assert_eq!(Session::length(), 3);
			assert_eq!(Session::current_index(), 1);

			// Block 5: Length now changed to 2. Index incremented.
			System::set_block_number(5);
			Session::check_rotate_session();
			assert_eq!(Session::length(), 2);
			assert_eq!(Session::current_index(), 2);

			// Block 6: No change.
			System::set_block_number(6);
			Session::check_rotate_session();
			assert_eq!(Session::length(), 2);
			assert_eq!(Session::current_index(), 2);

			// Block 7: Next index.
			System::set_block_number(7);
			Session::check_rotate_session();
			assert_eq!(Session::length(), 2);
			assert_eq!(Session::current_index(), 3);
		});
	}

	#[test]
	fn session_change_should_work() {
		with_externalities(&mut new_test_ext(), || {
			// Block 1: No change
			System::set_block_number(1);
			Session::check_rotate_session();
			assert_eq!(Consensus::authorities(), vec![1, 2, 3]);

			// Block 2: Session rollover, but no change.
			System::set_block_number(2);
			Session::check_rotate_session();
			assert_eq!(Consensus::authorities(), vec![1, 2, 3]);

			// Block 3: Set new key for validator 2; no visible change.
			System::set_block_number(3);
			assert_ok!(Session::set_key(&2, 5));
			assert_eq!(Consensus::authorities(), vec![1, 2, 3]);

			Session::check_rotate_session();
			assert_eq!(Consensus::authorities(), vec![1, 2, 3]);

			// Block 4: Session rollover, authority 2 changes.
			System::set_block_number(4);
			Session::check_rotate_session();
			assert_eq!(Consensus::authorities(), vec![1, 5, 3]);
		});
	}
}
